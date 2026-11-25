package com.x1f4r.legion

import java.io.File
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.util.Locale

/**
 * Pure-JVM security policy for Android file transfers:
 * - Cross-platform filename sanitization (union of Windows, POSIX, and Unicode Cc/Cf rules)
 * - Canonical directory containment checks for private staging/cache files
 * - Device-derived MIME table and refused executable/installer/script/shortcut/disk-image list
 * - Completed notification tap routing (always chooser for safe types; Downloads folder for refused types)
 * - Constant-time LocalSocket bridge hello token verification
 * - Open-externally cache pruning by age, count, and total size
 *
 * Kept free of `android.*` dependencies so every rule is covered by fast JVM unit tests.
 */
object TransferPathPolicy {
  const val MAX_FILE_NAME_UTF8_BYTES = 200
  const val MAX_OPEN_EXTERNALLY_CACHE_FILES = 24
  const val MAX_OPEN_EXTERNALLY_CACHE_AGE_MS = 24L * 60L * 60L * 1000L
  const val MAX_OPEN_EXTERNALLY_CACHE_BYTES = 256L * 1000L * 1000L

  private val REFUSED_OPEN_EXTENSIONS: Set<String> = setOf(
    // Android packages and native binaries
    "apk", "apks", "apkm", "xapk", "aab", "dex", "odex", "so",
    // Windows executables, installers, scripts, shortcuts, Office macros
    "exe", "dll", "sys", "msi", "msp", "msu", "mst", "com", "scr", "pif",
    "bat", "cmd", "ps1", "psm1", "psd1",
    "vbs", "vbe", "js", "jse", "mjs", "cjs", "wsf", "wsh", "hta",
    "cpl", "msc", "reg", "inf", "lnk", "url", "scf", "gadget",
    "settingcontent-ms", "appref-ms", "msix", "msixbundle", "appx", "appxbundle", "chm",
    "xll", "xlam", "docm", "xlsm", "pptm", "dotm",
    // Unix / macOS scripts, bundles, installers, packages, binaries, launchers
    "sh", "bash", "zsh", "ksh", "csh", "fish", "command", "tool",
    "app", "pkg", "mpkg", "action", "workflow", "applescript", "osascript", "scpt", "scptd",
    "mobileconfig", "terminal", "inetloc", "webloc", "macho", "dylib",
    "appimage", "deb", "rpm", "flatpak", "flatpakref", "snap",
    "run", "bin", "elf", "desktop",
    // Disk images that auto-mount or execute
    "dmg", "iso", "img", "vhd", "vhdx", "vmdk",
    // Cross-platform runnable bytecode / scripts
    "jar", "jnlp", "py", "pyc", "pyw", "rb", "pl", "php",
  )

  private val RESERVED_WINDOWS_STEMS: Set<String> = setOf(
    "CON", "PRN", "AUX", "NUL",
    "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
    "COM\u00B9", "COM\u00B2", "COM\u00B3",
    "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    "LPT\u00B9", "LPT\u00B2", "LPT\u00B3",
  )

  private const val FORBIDDEN_CHARS = "<>:\"/\\|?*"

  enum class NotificationTapTarget {
    OPEN_WITH_CHOOSER,
    OPEN_DOWNLOADS,
    OPEN_APP,
  }

  data class NotificationTapDecision(
    val target: NotificationTapTarget,
    val mime: String?,
    val title: String,
    val body: String,
  )

  /**
   * Sanitizes an untrusted filename into a single safe cross-platform path component.
   */
  fun sanitizeFileName(raw: String): String {
    // 1. Keep only the final path segment across POSIX and Windows separators.
    var s = raw
    val lastSep = maxOf(s.lastIndexOf('/'), s.lastIndexOf('\\'))
    if (lastSep >= 0) {
      s = s.substring(lastSep + 1)
    }
    // 2. Strip leading ASCII Windows drive designators (e.g. `C:version.dll`, `C:D:foo.txt`).
    while (s.length >= 2 && (s[0] in 'A'..'Z' || s[0] in 'a'..'z') && s[1] == ':') {
      s = s.substring(2)
    }
    // 3. Strip NTFS Alternate Data Stream suffix (`report.pdf:Zone.Identifier` -> `report.pdf`).
    val colonIdx = s.indexOf(':')
    if (colonIdx >= 0) {
      s = s.substring(0, colonIdx)
    }

    // 4. Drop forbidden characters, Unicode control (Cc), and Unicode format (Cf) characters.
    val sb = StringBuilder(s.length)
    var i = 0
    while (i < s.length) {
      val cp = s.codePointAt(i)
      val charCount = Character.charCount(cp)
      val type = Character.getType(cp)
      val isControlOrFormat =
        cp == 0 ||
          type == Character.CONTROL.toInt() ||
          type == Character.FORMAT.toInt()
      val isForbidden = charCount == 1 && FORBIDDEN_CHARS.indexOf(cp.toChar()) >= 0
      if (!isControlOrFormat && !isForbidden) {
        sb.appendCodePoint(cp)
      }
      i += charCount
    }

    var cleaned = sb.toString().trim { ch -> ch == '.' || ch.isWhitespace() }
    if (cleaned.isEmpty() || cleaned == "." || cleaned == "..") {
      return "download"
    }

    cleaned = truncateUtf8KeepingExtension(cleaned, MAX_FILE_NAME_UTF8_BYTES)
    cleaned = cleaned.trim { ch -> ch == '.' || ch.isWhitespace() }
    if (cleaned.isEmpty() || cleaned == "." || cleaned == "..") {
      return "download"
    }
    cleaned = neutralizeReservedWindowsName(cleaned)
    return truncateUtf8KeepingExtension(cleaned, MAX_FILE_NAME_UTF8_BYTES)
  }

  private fun neutralizeReservedWindowsName(name: String): String {
    val dot = name.indexOf('.')
    val firstStem = if (dot >= 0) name.substring(0, dot) else name
    val upper = firstStem.trimEnd(' ').uppercase(Locale.ROOT)
    return if (RESERVED_WINDOWS_STEMS.contains(upper)) "_$name" else name
  }

  private fun truncateUtf8KeepingExtension(name: String, maxBytes: Int): String {
    if (name.toByteArray(StandardCharsets.UTF_8).size <= maxBytes) {
      return name
    }
    val lastDot = name.lastIndexOf('.')
    val hasExt = lastDot > 0 && lastDot < name.length - 1
    if (hasExt) {
      val extPart = name.substring(lastDot) // includes '.'
      val extBytes = extPart.toByteArray(StandardCharsets.UTF_8).size
      if (extBytes < maxBytes - 1) {
        val stemBudget = maxBytes - extBytes
        val stemTrunc = truncateUtf8(name.substring(0, lastDot), stemBudget)
          .trimEnd { ch -> ch == '.' || ch.isWhitespace() }
        if (stemTrunc.isNotEmpty()) {
          return stemTrunc + extPart
        }
      }
    }
    return truncateUtf8(name, maxBytes)
  }

  private fun truncateUtf8(s: String, maxBytes: Int): String {
    val sb = StringBuilder()
    var used = 0
    var i = 0
    while (i < s.length) {
      val cp = s.codePointAt(i)
      val count = Character.charCount(cp)
      val piece = s.substring(i, i + count)
      val pieceBytes = piece.toByteArray(StandardCharsets.UTF_8).size
      if (used + pieceBytes > maxBytes) break
      sb.append(piece)
      used += pieceBytes
      i += count
    }
    return sb.toString()
  }

  /**
   * Returns the lowercase extension (without leading dot) of the sanitized filename, or empty.
   */
  fun extensionOf(fileName: String): String {
    val safe = sanitizeFileName(fileName)
    val dot = safe.lastIndexOf('.')
    if (dot <= 0 || dot >= safe.length - 1) return ""
    return safe.substring(dot + 1).lowercase(Locale.ROOT)
  }

  /**
   * Returns `true` if Legion refuses to open this file directly (executables, installers, scripts,
   * shortcuts, disk images, and macro-enabled office documents).
   */
  fun isRefusedOpenFileName(fileName: String): Boolean {
    val ext = extensionOf(fileName)
    return ext.isNotEmpty() && REFUSED_OPEN_EXTENSIONS.contains(ext)
  }

  /**
   * Derives a safe MIME type strictly on this device from the filename's real extension.
   * Never trusts MIME strings sent by a remote machine.
   */
  fun mimeForFileName(fileName: String): String {
    return when (extensionOf(fileName)) {
      "jpg", "jpeg" -> "image/jpeg"
      "png" -> "image/png"
      "gif" -> "image/gif"
      "webp" -> "image/webp"
      "avif" -> "image/avif"
      "bmp" -> "image/bmp"
      "heic" -> "image/heic"
      "heif" -> "image/heif"
      "tif", "tiff" -> "image/tiff"
      "svg" -> "image/svg+xml"
      "mp4", "m4v" -> "video/mp4"
      "webm" -> "video/webm"
      "mov" -> "video/quicktime"
      "mkv" -> "video/x-matroska"
      "avi" -> "video/x-msvideo"
      "mp3" -> "audio/mpeg"
      "m4a", "aac" -> "audio/mp4"
      "flac" -> "audio/flac"
      "ogg", "opus" -> "audio/ogg"
      "wav" -> "audio/wav"
      "pdf" -> "application/pdf"
      "epub" -> "application/epub+zip"
      "txt", "md", "log", "csv", "toml", "yaml", "yml", "vtt" -> "text/plain"
      "json" -> "application/json"
      "xml" -> "application/xml"
      "html", "htm" -> "text/html"
      "css" -> "text/css"
      "zip" -> "application/zip"
      "tar" -> "application/x-tar"
      "gz" -> "application/gzip"
      "xz" -> "application/x-xz"
      "zst" -> "application/zstd"
      "7z" -> "application/x-7z-compressed"
      "rar" -> "application/vnd.rar"
      "doc" -> "application/msword"
      "docx" -> "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
      "xls" -> "application/vnd.ms-excel"
      "xlsx" -> "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
      "ppt" -> "application/vnd.ms-powerpoint"
      "pptx" -> "application/vnd.openxmlformats-officedocument.presentationml.presentation"
      "odt" -> "application/vnd.oasis.opendocument.text"
      "ods" -> "application/vnd.oasis.opendocument.spreadsheet"
      "odp" -> "application/vnd.oasis.opendocument.presentation"
      else -> "application/octet-stream"
    }
  }

  /**
   * Verifies that `candidate` resolves canonically inside `rootDir` (and is a regular file when
   * `requireExistingFile` is true), refusing traversal, prefix tricks (`parts2`), and symlink escapes.
   */
  fun resolveContainedChild(
    rootDir: File,
    candidate: File,
    requireExistingFile: Boolean = true,
  ): File {
    val canonicalRoot = rootDir.canonicalFile
    val canonicalCandidate = candidate.canonicalFile
    val rootPrefix = canonicalRoot.path.trimEnd(File.separatorChar) + File.separatorChar
    if (!canonicalCandidate.path.startsWith(rootPrefix)) {
      throw SecurityException("Refusing transfer path outside ${canonicalRoot.name}.")
    }
    if (requireExistingFile && !canonicalCandidate.isFile) {
      throw SecurityException("Refusing missing or non-regular transfer file.")
    }
    return canonicalCandidate
  }

  /**
   * Constructs a safe destination file directly inside `cacheDir` and verifies containment.
   */
  fun resolveNewCacheFile(cacheDir: File, transferId: String, fileName: String): File {
    val safeId = sanitizeFileName(transferId)
    val safeName = sanitizeFileName(fileName)
    val combined = sanitizeFileName("$safeId-$safeName")
    val canonicalDir = cacheDir.canonicalFile
    val target = File(canonicalDir, combined)
    val canonicalParent = target.parentFile?.canonicalFile
    if (canonicalParent != canonicalDir) {
      throw SecurityException("Refusing cache file outside open-externally directory.")
    }
    return target
  }

  /**
   * Atomically claims a brand-new cache file inside `cacheDir` using `createNewFile()` (`O_EXCL`)
   * without ever overwriting an existing file, trying numbered siblings on collision.
   */
  fun createUniqueCacheFile(cacheDir: File, transferId: String, fileName: String): File {
    val base = resolveNewCacheFile(cacheDir, transferId, fileName)
    val canonicalDir = cacheDir.canonicalFile
    if (base.createNewFile()) {
      return base
    }
    val baseName = base.name
    val dot = baseName.lastIndexOf('.')
    val stem = if (dot > 0 && dot < baseName.length - 1) baseName.substring(0, dot) else baseName
    val ext = if (dot > 0 && dot < baseName.length - 1) baseName.substring(dot) else ""
    for (attempt in 2..9999) {
      val candidateName = sanitizeFileName("$stem ($attempt)$ext")
      val candidate = File(canonicalDir, candidateName)
      if (candidate.parentFile?.canonicalFile != canonicalDir) {
        throw SecurityException("Refusing cache file outside open-externally directory.")
      }
      if (candidate.createNewFile()) {
        return candidate
      }
    }
    throw IllegalStateException("Could not allocate a unique cache file for $baseName.")
  }

  /**
   * Constant-time verification of the LocalSocket bridge `hello` handshake fields.
   */
  fun verifyBridgeHello(typeField: String?, providedToken: String?, expectedToken: String): Boolean {
    if (typeField != "hello") return false
    if (providedToken.isNullOrEmpty() || expectedToken.isEmpty()) return false
    val a = providedToken.toByteArray(StandardCharsets.UTF_8)
    val b = expectedToken.toByteArray(StandardCharsets.UTF_8)
    return MessageDigest.isEqual(a, b)
  }

  /**
   * Decides what tapping a completed transfer notification should do.
   *
   * - Uploads open Legion.
   * - Downloads with refused executable/installer extensions open the Downloads folder (never
   *   launching a package installer or script).
   * - Safe downloads with a `content://` URI open via `Intent.createChooser` using the
   *   device-derived MIME type (ignoring any remote MIME claim).
   */
  fun decideCompletedNotificationTap(
    direction: String,
    fileName: String,
    localUri: String,
  ): NotificationTapDecision {
    val safeName = sanitizeFileName(fileName)
    if (direction == "upload") {
      return NotificationTapDecision(
        target = NotificationTapTarget.OPEN_APP,
        mime = null,
        title = "Uploaded $safeName",
        body = "Finished uploading",
      )
    }
    if (isRefusedOpenFileName(safeName)) {
      return NotificationTapDecision(
        target = NotificationTapTarget.OPEN_DOWNLOADS,
        mime = null,
        title = "Downloaded $safeName",
        body = "Saved in Downloads/Legion · Open from Files",
      )
    }
    if (localUri.startsWith("content://")) {
      return NotificationTapDecision(
        target = NotificationTapTarget.OPEN_WITH_CHOOSER,
        mime = mimeForFileName(safeName),
        title = "Downloaded $safeName",
        body = "Saved in Downloads/Legion",
      )
    }
    return NotificationTapDecision(
      target = NotificationTapTarget.OPEN_APP,
      mime = null,
      title = "Downloaded $safeName",
      body = "Saved in Downloads/Legion",
    )
  }

  /**
   * Prunes cached files in `dir` by age (`maxAgeMs`), count (`maxCount`), and total size
   * (`maxTotalBytes`), deleting expired and oldest files first.
   */
  fun pruneCacheDirectory(
    dir: File,
    nowMs: Long = System.currentTimeMillis(),
    maxCount: Int = MAX_OPEN_EXTERNALLY_CACHE_FILES,
    maxAgeMs: Long = MAX_OPEN_EXTERNALLY_CACHE_AGE_MS,
    maxTotalBytes: Long = MAX_OPEN_EXTERNALLY_CACHE_BYTES,
  ): Int {
    val entries = dir.listFiles()?.filter { it.isFile } ?: return 0
    var removed = 0
    val live = mutableListOf<File>()
    for (file in entries) {
      val ageMs = (nowMs - file.lastModified()).coerceAtLeast(0L)
      if (ageMs > maxAgeMs) {
        if (file.delete()) removed++
      } else {
        live.add(file)
      }
    }
    live.sortByDescending { it.lastModified() }
    var keptCount = 0
    var keptBytes = 0L
    for (file in live) {
      val len = file.length().coerceAtLeast(0L)
      val overCount = keptCount >= maxCount
      val overSize = keptCount > 0 && keptBytes + len > maxTotalBytes
      if (overCount || overSize) {
        if (file.delete()) removed++
      } else {
        keptCount++
        keptBytes += len
      }
    }
    return removed
  }
}

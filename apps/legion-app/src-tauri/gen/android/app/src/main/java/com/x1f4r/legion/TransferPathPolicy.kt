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
  const val MAX_EXTENSION_BYTES = 32
  const val MAX_OPEN_EXTERNALLY_CACHE_FILES = 24
  const val MAX_OPEN_EXTERNALLY_CACHE_AGE_MS = 24L * 60L * 60L * 1000L
  const val MAX_OPEN_EXTERNALLY_CACHE_BYTES = 256L * 1000L * 1000L

  // Explicit direct-open allow-list (mirrored 1-to-1 with `file_policy.rs::ALLOWED_OPEN_MIME_BY_EXT`).
  // BEGIN_ALLOWED_OPEN_MIME_BY_EXT
  internal val ALLOWED_OPEN_MIME_BY_EXT: Map<String, String> = mapOf(
    // Pictures (NOT SVG)
    "jpg" to "image/jpeg",
    "jpeg" to "image/jpeg",
    "png" to "image/png",
    "gif" to "image/gif",
    "webp" to "image/webp",
    "avif" to "image/avif",
    "bmp" to "image/bmp",
    "heic" to "image/heic",
    "heif" to "image/heif",
    "tif" to "image/tiff",
    "tiff" to "image/tiff",
    "ico" to "image/x-icon",
    // Audio
    "mp3" to "audio/mpeg",
    "m4a" to "audio/mp4",
    "m4b" to "audio/mp4",
    "aac" to "audio/mp4",
    "flac" to "audio/flac",
    "ogg" to "audio/ogg",
    "opus" to "audio/ogg",
    "wav" to "audio/wav",
    "aif" to "audio/aiff",
    "aiff" to "audio/aiff",
    // Video
    "mp4" to "video/mp4",
    "m4v" to "video/mp4",
    "webm" to "video/webm",
    "mov" to "video/quicktime",
    "mkv" to "video/x-matroska",
    "avi" to "video/x-msvideo",
    // PDF
    "pdf" to "application/pdf",
    // Plain text and plain-text family (NOT HTML, XML, SVG)
    "txt" to "text/plain",
    "md" to "text/plain",
    "log" to "text/plain",
    "toml" to "text/plain",
    "yaml" to "text/plain",
    "yml" to "text/plain",
    "ini" to "text/plain",
    "conf" to "text/plain",
    "vtt" to "text/plain",
    "json" to "application/json",
    // Common macro-free document formats
    "csv" to "text/plain",
    "docx" to "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "xlsx" to "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "pptx" to "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    "odt" to "application/vnd.oasis.opendocument.text",
    "ods" to "application/vnd.oasis.opendocument.spreadsheet",
    "odp" to "application/vnd.oasis.opendocument.presentation",
    "epub" to "application/epub+zip",
  )
  // END_ALLOWED_OPEN_MIME_BY_EXT

  // Non-openable download MIME mappings (mirrored 1-to-1 with `file_policy.rs::NON_OPEN_DOWNLOAD_MIME_BY_EXT`).
  // BEGIN_NON_OPEN_DOWNLOAD_MIME_BY_EXT
  internal val NON_OPEN_DOWNLOAD_MIME_BY_EXT: Map<String, String> = mapOf(
    "svg" to "image/svg+xml",
    "xml" to "application/xml",
    "html" to "text/html",
    "htm" to "text/html",
    "css" to "text/css",
    "rtf" to "application/rtf",
    "zip" to "application/zip",
    "tar" to "application/x-tar",
    "gz" to "application/gzip",
    "bz2" to "application/x-bzip2",
    "xz" to "application/x-xz",
    "zst" to "application/zstd",
    "7z" to "application/x-7z-compressed",
    "rar" to "application/vnd.rar",
    "doc" to "application/msword",
    "xls" to "application/vnd.ms-excel",
    "ppt" to "application/vnd.ms-powerpoint",
  )
  // END_NON_OPEN_DOWNLOAD_MIME_BY_EXT

  // Defense-in-depth refused extension list (mirrored 1-to-1 with `file_policy.rs::REFUSED_OPEN_EXTENSIONS`).
  // BEGIN_REFUSED_OPEN_EXTENSIONS
  internal val REFUSED_OPEN_EXTENSIONS: Set<String> = setOf(
    // Android packages and native binaries
    "apk", "apks", "apkm", "xapk", "aab", "dex", "odex", "so",
    // Windows executables, installers, scripts, shortcuts, profiles, and Office macro carriers
    "exe", "dll", "sys", "scr", "com", "msi", "msp", "msu", "mst",
    "bat", "cmd", "ps1", "psm1", "psd1", "psc1", "ps1xml", "msh",
    "vbs", "vbe", "js", "jse", "mjs", "cjs", "ws", "wsc", "wsf", "wsh", "sct", "hta",
    "cpl", "msc", "inf", "reg", "lnk", "url", "pif", "scf", "gadget",
    "application", "rdp", "diagcab",
    "settingcontent-ms", "appref-ms", "library-ms", "search-ms", "searchconnector-ms",
    "theme", "themepack", "website", "xbap", "vsto", "cer",
    "msix", "msixbundle", "appx", "appxbundle", "jar", "jnlp", "chm",
    "xll", "xla", "xlam", "docm", "xlsm", "xlsb", "pptm", "ppsm", "potm", "xltm", "ppam", "dotm",
    "mde", "accde", "mdb", "iqy", "slk",
    // macOS executables, bundles, installers, scripts, shortcuts, and disk images
    "app", "pkg", "mpkg", "dmg", "command", "tool", "workflow", "action",
    "applescript", "osascript", "scpt", "scptd",
    "mobileconfig", "terminal", "inetloc", "webloc", "fileloc", "shortcut", "macho", "dylib",
    // Linux / cross-platform scripts, executables, packages, launchers, and disk images
    "sh", "bash", "zsh", "fish", "csh", "ksh",
    "run", "bin", "elf", "appimage", "deb", "rpm",
    "flatpak", "flatpakref", "flatpakrepo", "snap", "desktop", "kdelnk", "ymp",
    "iso", "img", "vhd", "vhdx", "vmdk",
    "py", "pyc", "pyw", "pyz", "pyzw", "rb", "pl", "php",
  )
  // END_REFUSED_OPEN_EXTENSIONS

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
   * Returns the lowercase pure ASCII-alphanumeric extension (without leading dot) of the sanitized
   * filename, or empty if absent or if any character lies outside ASCII letters and digits
   * (`A-Z`, `a-z`, `0-9`).
   *
   * Any non-ASCII extension (such as Kelvin sign `x.ap\u212A`) returns empty (unknown type),
   * keeping Kotlin and Rust 100% consistent.
   */
  fun extensionOf(fileName: String): String {
    val safe = sanitizeFileName(fileName)
    val dot = safe.lastIndexOf('.')
    if (dot <= 0 || dot >= safe.length - 1) return ""
    val ext = safe.substring(dot + 1)
    if (ext.length > MAX_EXTENSION_BYTES) return ""
    for (i in 0 until ext.length) {
      val c = ext[i]
      val asciiAlnum = (c in 'A'..'Z') || (c in 'a'..'z') || (c in '0'..'9')
      if (!asciiAlnum) return ""
    }
    return ext.lowercase(Locale.ROOT)
  }

  private fun refusalExtensionOf(fileName: String): String {
    val safe = sanitizeFileName(fileName)
    val dot = safe.lastIndexOf('.')
    if (dot <= 0 || dot >= safe.length - 1) return ""
    val ext = safe.substring(dot + 1)
    if (ext.length > MAX_EXTENSION_BYTES) return ""
    for (i in 0 until ext.length) {
      val c = ext[i]
      val allowed = (c in 'A'..'Z') || (c in 'a'..'z') || (c in '0'..'9') || c == '-'
      if (!allowed) return ""
    }
    return ext.lowercase(Locale.ROOT)
  }

  /**
   * Returns `true` if `fileName` has an extension on the defense-in-depth refused list.
   */
  fun isRefusedOpenFileName(fileName: String): Boolean {
    val ext = refusalExtensionOf(fileName)
    return ext.isNotEmpty() && REFUSED_OPEN_EXTENSIONS.contains(ext)
  }

  /**
   * Returns `true` ONLY when `fileName` has a pure ASCII-alphanumeric extension on the explicit
   * direct-open allow-list and is NOT on the defense-in-depth refused list.
   */
  fun canOpenFileName(fileName: String): Boolean {
    if (isRefusedOpenFileName(fileName)) return false
    val ext = extensionOf(fileName)
    if (ext.isEmpty() || REFUSED_OPEN_EXTENSIONS.contains(ext)) return false
    return ALLOWED_OPEN_MIME_BY_EXT.containsKey(ext)
  }

  /**
   * Derives a safe MIME type strictly on this device from the filename's real ASCII extension.
   * Never trusts MIME strings sent by a remote machine.
   */
  fun mimeForFileName(fileName: String): String {
    if (isRefusedOpenFileName(fileName)) return "application/octet-stream"
    val ext = extensionOf(fileName)
    if (ext.isEmpty() || REFUSED_OPEN_EXTENSIONS.contains(ext)) return "application/octet-stream"
    return ALLOWED_OPEN_MIME_BY_EXT[ext]
      ?: NON_OPEN_DOWNLOAD_MIME_BY_EXT[ext]
      ?: "application/octet-stream"
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
   * Returns `true` only for `content://` authorities that Legion itself created for a saved
   * download (`media` for MediaStore Downloads, or `<packageName>.fileprovider` for our
   * open-externally cache), preventing arbitrary document URIs from ever being deleted.
   */
  fun isDeletableSavedDownloadUri(authority: String?, packageName: String): Boolean {
    if (authority.isNullOrBlank()) return false
    return authority == "media" || authority == "$packageName.fileprovider"
  }

  /**
   * Deletes `candidate` ONLY if it resolves canonically strictly inside `cacheDir` (refusing
   * traversal, sibling-prefix tricks, and symlink escapes). Returns `true` if a file was removed.
   */
  fun deleteContainedCacheFile(cacheDir: File, candidate: File): Boolean {
    val resolved = resolveContainedChild(cacheDir, candidate, requireExistingFile = false)
    return resolved.isFile && resolved.delete()
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
    if (!canOpenFileName(safeName)) {
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

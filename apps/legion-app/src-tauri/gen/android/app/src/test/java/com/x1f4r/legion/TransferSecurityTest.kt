package com.x1f4r.legion

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.nio.charset.StandardCharsets
import java.nio.file.Files

class TransferSecurityTest {
  @get:Rule
  val tempFolder = TemporaryFolder()

  @Test
  fun sanitizerNeutralizesHostileWindowsPosixAndUnicodeNames() {
    assertEquals("download", TransferPathPolicy.sanitizeFileName(""))
    assertEquals("download", TransferPathPolicy.sanitizeFileName("..."))
    assertEquals("download", TransferPathPolicy.sanitizeFileName("   ..   "))
    assertEquals("passwd", TransferPathPolicy.sanitizeFileName("../../etc/passwd"))
    assertEquals("win.ini", TransferPathPolicy.sanitizeFileName("C:\\Windows\\win.ini"))
    assertEquals("version.dll", TransferPathPolicy.sanitizeFileName("dir/C:version.dll"))
    assertEquals("payload.exe", TransferPathPolicy.sanitizeFileName("C:D:payload.exe"))
    assertEquals("report.pdf", TransferPathPolicy.sanitizeFileName("report.pdf:Zone.Identifier"))
    assertEquals("_CON", TransferPathPolicy.sanitizeFileName("CON"))
    assertEquals("_nul.txt", TransferPathPolicy.sanitizeFileName("nul.txt"))
    assertEquals("_COM1.tar.gz", TransferPathPolicy.sanitizeFileName("COM1.tar.gz"))
    assertEquals("_LPT\u00B2.log", TransferPathPolicy.sanitizeFileName("LPT\u00B2.log"))

    // BiDi RLO spoofing (U+202E) is stripped so the real extension stays visible.
    val bidi = "invoice_\u202Egpj.apk"
    val sanitizedBidi = TransferPathPolicy.sanitizeFileName(bidi)
    assertFalse(sanitizedBidi.contains('\u202E'))
    assertTrue(sanitizedBidi.endsWith(".apk"))
    assertTrue(TransferPathPolicy.isRefusedOpenFileName(sanitizedBidi))

    // Overlong UTF-8 filename preserves extension and stays within byte limit.
    val longName = "a".repeat(260) + ".mp4"
    val truncated = TransferPathPolicy.sanitizeFileName(longName)
    assertTrue(truncated.toByteArray(StandardCharsets.UTF_8).size <= TransferPathPolicy.MAX_FILE_NAME_UTF8_BYTES)
    assertTrue(truncated.endsWith(".mp4"))

    // Forbidden characters replaced cleanly.
    val messy = "a<b>c:d\"e|f?g*h.txt"
    val cleaned = TransferPathPolicy.sanitizeFileName(messy)
    assertFalse(cleaned.any { it in "<>:\"/\\|?*" })
  }

  @Test
  fun pathContainmentRejectsTraversalPrefixTricksAndSymlinkEscapes() {
    val root = tempFolder.newFolder("app-data")
    val partsDir = File(root, "transfers/parts").apply { mkdirs() }
    val validPart = File(partsDir, "tr-1.part").apply { writeText("ok") }

    val resolved = TransferPathPolicy.resolveContainedChild(partsDir, validPart)
    assertEquals(validPart.canonicalFile, resolved)

    // Sibling prefix trick (`parts2/evil.part`) must be rejected.
    val siblingDir = File(root, "transfers/parts2").apply { mkdirs() }
    val siblingFile = File(siblingDir, "evil.part").apply { writeText("escape") }
    assertThrows(SecurityException::class.java) {
      TransferPathPolicy.resolveContainedChild(partsDir, siblingFile)
    }

    // `..` traversal out of parts directory must be rejected.
    val secret = File(root, "legion-app.json").apply { writeText("keys") }
    assertThrows(SecurityException::class.java) {
      TransferPathPolicy.resolveContainedChild(partsDir, File(partsDir, "../../legion-app.json"))
    }

    // Symlink inside partsDir pointing to secret outside partsDir must be rejected.
    val linkFile = File(partsDir, "symlink.part")
    Files.createSymbolicLink(linkFile.toPath(), secret.toPath())
    assertThrows(SecurityException::class.java) {
      TransferPathPolicy.resolveContainedChild(partsDir, linkFile)
    }

    // New cache file stays strictly inside open-externally and never overwrites existing entries.
    val cacheDir = File(root, "open-externally").apply { mkdirs() }
    val first = TransferPathPolicy.createUniqueCacheFile(cacheDir, "../../id", "..\\..\\evil.jpg")
    first.writeText("first")
    val second = TransferPathPolicy.createUniqueCacheFile(cacheDir, "../../id", "..\\..\\evil.jpg")
    second.writeText("second")
    assertEquals(cacheDir.canonicalFile, first.parentFile?.canonicalFile)
    assertEquals(cacheDir.canonicalFile, second.parentFile?.canonicalFile)
    assertNotEquals(first, second)
    assertEquals("first", first.readText())
    assertEquals("second", second.readText())
    assertTrue(first.name.endsWith(".jpg"))
    assertTrue(second.name.endsWith(".jpg"))
  }

  @Test
  fun bridgeHelloHandshakeVerifiesTokenStrictly() {
    val secret = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    assertTrue(TransferPathPolicy.verifyBridgeHello("hello", secret, secret))
    assertFalse(TransferPathPolicy.verifyBridgeHello("hello", secret.dropLast(1) + "0", secret))
    assertFalse(TransferPathPolicy.verifyBridgeHello("hello", "", secret))
    assertFalse(TransferPathPolicy.verifyBridgeHello("hello", null, secret))
    assertFalse(TransferPathPolicy.verifyBridgeHello("other", secret, secret))
    assertFalse(TransferPathPolicy.verifyBridgeHello(null, secret, secret))
    assertFalse(TransferPathPolicy.verifyBridgeHello("hello", secret, ""))
  }

  @Test
  fun refusedExtensionsAndNotificationTapDecisionsIgnoreRemoteMime() {
    // Executables, installers, scripts, shortcuts, disk images, archives, web pages, and unknown
    // extensions are NEVER directly openable; tapping the completion notification opens Downloads.
    for (blocked in listOf(
      "evil.apk",
      "bundle.APKS",
      "setup.exe",
      "install.msi",
      "run.sh",
      "payload.ps1",
      "disk.dmg",
      "image.iso",
      "shortcut.lnk",
      "macro.docm",
      "tool.jar",
      "page.html",
      "page.htm",
      "vector.svg",
      "feed.xml",
      "archive.zip",
      "backup.tar",
      "backup.gz",
      "legacy.doc",
      "legacy.xls",
      "legacy.ppt",
      "legacy.rtf",
      "blob.unknown",
      "README",
      "x.ap\u212A",
      "run.pyz",
      "run.pyzw",
      "script.ws",
      "script.wsc",
      "script.sct",
      "clickonce.application",
      "remote.rdp",
      "diag.diagcab",
      "lib.library-ms",
      "query.search-ms",
      "query.searchConnector-ms",
      "look.theme",
      "look.themepack",
      "pin.website",
      "app.xbap",
      "addin.vsto",
      "console.psc1",
      "types.ps1xml",
      "shell.msh",
      "cert.cer",
      "sheet.xlsb",
      "slides.ppsm",
      "tmpl.potm",
      "tmpl.xltm",
      "addin.ppam",
      "addin.xla",
      "db.mde",
      "db.accde",
      "db.mdb",
      "web.iqy",
      "sheet.slk",
      "mac.fileloc",
      "mac.shortcut",
      "kde.kdelnk",
      "suse.ymp",
      "repo.flatpakrepo",
    )) {
      assertFalse("expected $blocked to not be directly openable", TransferPathPolicy.canOpenFileName(blocked))
      val decision = TransferPathPolicy.decideCompletedNotificationTap(
        direction = "download",
        fileName = blocked,
        localUri = "content://media/external/downloads/42",
      )
      assertEquals(
        "non-allowed type $blocked must open Downloads instead of viewer/installer",
        TransferPathPolicy.NotificationTapTarget.OPEN_DOWNLOADS,
        decision.target,
      )
      assertNull(decision.mime)
    }

    // Explicit Kelvin sign check (`x.ap\u212A`): non-ASCII extension is treated as unknown type
    // matching Rust exactly (never lowercased to `apk` and never openable).
    val kelvin = "x.ap\u212A"
    assertEquals("", TransferPathPolicy.extensionOf(kelvin))
    assertFalse(TransferPathPolicy.canOpenFileName(kelvin))
    assertFalse(TransferPathPolicy.isRefusedOpenFileName(kelvin))
    assertEquals("application/octet-stream", TransferPathPolicy.mimeForFileName(kelvin))

    // Safe media / document files derive MIME on device and always open via chooser.
    val photoDecision = TransferPathPolicy.decideCompletedNotificationTap(
      direction = "download",
      fileName = "holiday.jpg",
      localUri = "content://media/external/downloads/99",
    )
    assertEquals(TransferPathPolicy.NotificationTapTarget.OPEN_WITH_CHOOSER, photoDecision.target)
    assertEquals("image/jpeg", photoDecision.mime)
    assertEquals("Downloaded holiday.jpg", photoDecision.title)

    val pdfDecision = TransferPathPolicy.decideCompletedNotificationTap(
      direction = "download",
      fileName = "invoice.PDF",
      localUri = "content://media/external/downloads/100",
    )
    assertEquals(TransferPathPolicy.NotificationTapTarget.OPEN_WITH_CHOOSER, pdfDecision.target)
    assertEquals("application/pdf", pdfDecision.mime)

    // Uploads open the Legion app rather than a file viewer.
    val uploadDecision = TransferPathPolicy.decideCompletedNotificationTap(
      direction = "upload",
      fileName = "clip.mp4",
      localUri = "content://com.android.providers/clip.mp4",
    )
    assertEquals(TransferPathPolicy.NotificationTapTarget.OPEN_APP, uploadDecision.target)
    assertEquals("Uploaded clip.mp4", uploadDecision.title)
  }

  @Test
  fun cachePruningEnforcesAgeCountAndTotalSizeBounds() {
    val cacheDir = tempFolder.newFolder("open-externally-cache")
    val now = 1_750_000_000_000L

    // Expired file (25 hours old) must be evicted first.
    val expired = File(cacheDir, "old.jpg").apply {
      writeBytes(ByteArray(1024))
      setLastModified(now - 25L * 60L * 60L * 1000L)
    }

    // Five recent 1000-byte files under a 2500-byte / 3-file cap: only the 2 newest fit (2000 <= 2500).
    val files = (1..5).map { idx ->
      File(cacheDir, "recent-$idx.jpg").apply {
        writeBytes(ByteArray(1000))
        setLastModified(now - (6 - idx) * 1000L)
      }
    }

    val deleted = TransferPathPolicy.pruneCacheDirectory(
      dir = cacheDir,
      nowMs = now,
      maxCount = 3,
      maxAgeMs = 24L * 60L * 60L * 1000L,
      maxTotalBytes = 2500L,
    )
    assertEquals(4, deleted)
    assertFalse(expired.exists())
    assertFalse(files[0].exists())
    assertFalse(files[1].exists())
    assertFalse(files[2].exists())
    assertTrue(files[3].exists())
    assertTrue(files[4].exists())
  }

  @Test
  fun openAllowListPropertyTestOver10000GeneratedNames() {
    val allowedKeys = TransferPathPolicy.ALLOWED_OPEN_MIME_BY_EXT.keys.toList()
    val refusedList = TransferPathPolicy.REFUSED_OPEN_EXTENSIONS.toList()
    val prefixes = listOf("", "dir/", "C:\\Users\\me\\", "../../", "C:", "\u202E", "  .")
    val stems = listOf(
      "file",
      "holiday (2)",
      "CON",
      "archive.tar",
      "photo.jpg",
      "doc\u202Egpj",
      "report_2026",
      "日本語",
      "x",
    )
    val exoticChars = charArrayOf(
      'a', 'z', 'A', 'Z', '0', '9', '-', '_', '.', ' ', '\u212A', '\u0130', '\u0131',
      '\u0430', '\u202E', 'é',
    )
    val extraExts = listOf(
      "html", "svg", "xml", "zip", "tar", "gz", "doc", "xls", "ppt", "rtf",
      "apk", "exe", "rdp", "xlsb", "search-ms", "ap\u212A", "jp\u0430", "pd\u017F", "unknown", "",
    )

    var seed = -7046029254386353131L // deterministic xorshift64
    fun nextLong(): Long {
      seed = seed xor (seed shl 13)
      seed = seed xor (seed ushr 7)
      seed = seed xor (seed shl 17)
      return seed
    }
    fun nextIdx(bound: Int): Int = ((nextLong() ushr 1) % bound.toLong()).toInt()

    var openableCount = 0
    var blockedCount = 0
    val total = 10_500

    for (i in 0 until total) {
      val prefix = prefixes[nextIdx(prefixes.size)]
      val stem = stems[nextIdx(stems.size)]
      val candidate = when (i % 5) {
        0 -> {
          val ext = allowedKeys[nextIdx(allowedKeys.size)]
          val cased = if ((nextLong() and 1L) == 0L) ext.uppercase() else ext
          val trail = if ((nextLong() and 3L) == 0L) ". " else ""
          "$prefix$stem.$cased$trail"
        }
        1 -> {
          val ext = refusedList[nextIdx(refusedList.size)]
          val ads = if ((nextLong() and 3L) == 0L) ":\$DATA" else ""
          "$prefix$stem.$ext$ads"
        }
        2 -> {
          val ext = extraExts[nextIdx(extraExts.size)]
          if (ext.isEmpty()) "$prefix$stem" else "$prefix$stem.$ext"
        }
        3 -> {
          val len = 1 + nextIdx(8)
          val sb = StringBuilder(len)
          repeat(len) {
            val idx = nextIdx(62)
            val ch = when {
              idx < 26 -> ('a'.code + idx).toChar()
              idx < 52 -> ('A'.code + (idx - 26)).toChar()
              else -> ('0'.code + (idx - 52)).toChar()
            }
            sb.append(ch)
          }
          "$prefix$stem.$sb"
        }
        else -> {
          val len = 1 + nextIdx(10)
          val sb = StringBuilder(len)
          repeat(len) {
            sb.append(exoticChars[nextIdx(exoticChars.size)])
          }
          "$prefix$stem.$sb"
        }
      }

      if (TransferPathPolicy.canOpenFileName(candidate)) {
        openableCount++
        val ext = TransferPathPolicy.extensionOf(candidate)
        assertTrue("openable $candidate must have non-empty ext", ext.isNotEmpty())
        assertTrue(
          "openable $candidate ext '$ext' must be lowercase ASCII alphanumeric",
          ext.all { it in 'a'..'z' || it in '0'..'9' },
        )
        assertTrue(
          "openable $candidate ext '$ext' must be on allow-list",
          TransferPathPolicy.ALLOWED_OPEN_MIME_BY_EXT.containsKey(ext),
        )
        assertFalse(
          "openable $candidate must not match refused list",
          TransferPathPolicy.REFUSED_OPEN_EXTENSIONS.contains(ext) ||
            TransferPathPolicy.isRefusedOpenFileName(candidate),
        )
      } else {
        blockedCount++
      }
    }

    assertTrue(
      "property test must cover both openable ($openableCount) and blocked ($blockedCount) cases",
      openableCount >= 1000 && blockedCount >= 5000,
    )
  }

  @Test
  fun cancelCleanupDeletesContainedCacheFilesAndGatesContentUriAuthorities() {
    // Only MediaStore Downloads (`media`) and our own FileProvider authority may be deleted.
    assertTrue(TransferPathPolicy.isDeletableSavedDownloadUri("media", "com.x1f4r.legion"))
    assertTrue(TransferPathPolicy.isDeletableSavedDownloadUri("com.x1f4r.legion.fileprovider", "com.x1f4r.legion"))
    assertTrue(TransferPathPolicy.isDeletableSavedDownloadUri("com.x1f4r.legion.debug.fileprovider", "com.x1f4r.legion.debug"))
    assertFalse(TransferPathPolicy.isDeletableSavedDownloadUri("com.android.externalstorage.documents", "com.x1f4r.legion"))
    assertFalse(TransferPathPolicy.isDeletableSavedDownloadUri("com.other.app.fileprovider", "com.x1f4r.legion"))
    assertFalse(TransferPathPolicy.isDeletableSavedDownloadUri("", "com.x1f4r.legion"))
    assertFalse(TransferPathPolicy.isDeletableSavedDownloadUri(null, "com.x1f4r.legion"))

    val root = tempFolder.newFolder("cancel-cleanup")
    val cacheDir = File(root, "open-externally").apply { mkdirs() }
    val cached = File(cacheDir, "tr-1-holiday.jpg").apply { writeText("cached-bytes") }
    assertTrue(TransferPathPolicy.deleteContainedCacheFile(cacheDir, cached))
    assertFalse(cached.exists())
    // Second call on missing file is a clean no-op.
    assertFalse(TransferPathPolicy.deleteContainedCacheFile(cacheDir, cached))

    // Traversal, sibling-prefix, and symlink escapes must throw SecurityException and leave target untouched.
    val outside = File(root, "secret.txt").apply { writeText("keep") }
    assertThrows(SecurityException::class.java) {
      TransferPathPolicy.deleteContainedCacheFile(cacheDir, File(cacheDir, "../secret.txt"))
    }
    val siblingDir = File(root, "open-externally2").apply { mkdirs() }
    val siblingFile = File(siblingDir, "other.jpg").apply { writeText("keep2") }
    assertThrows(SecurityException::class.java) {
      TransferPathPolicy.deleteContainedCacheFile(cacheDir, siblingFile)
    }
    val link = File(cacheDir, "escape-link.jpg")
    Files.createSymbolicLink(link.toPath(), outside.toPath())
    assertThrows(SecurityException::class.java) {
      TransferPathPolicy.deleteContainedCacheFile(cacheDir, link)
    }
    assertTrue(outside.exists())
    assertTrue(siblingFile.exists())
  }
}

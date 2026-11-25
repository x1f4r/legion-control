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

    // New cache file stays strictly inside open-externally even with hostile transferId / fileName.
    val cacheDir = File(root, "open-externally").apply { mkdirs() }
    val cacheTarget = TransferPathPolicy.resolveNewCacheFile(cacheDir, "../../id", "..\\..\\evil.jpg")
    assertEquals(cacheDir.canonicalFile, cacheTarget.parentFile?.canonicalFile)
    assertNotEquals("evil.jpg", cacheTarget.name)
    assertTrue(cacheTarget.name.endsWith(".jpg"))
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
    // Executables, installers, scripts, shortcuts, disk images are always refused.
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
    )) {
      assertTrue("expected $blocked to be refused", TransferPathPolicy.isRefusedOpenFileName(blocked))
      val decision = TransferPathPolicy.decideCompletedNotificationTap(
        direction = "download",
        fileName = blocked,
        localUri = "content://media/external/downloads/42",
      )
      assertEquals(
        "refused type $blocked must open Downloads instead of viewer/installer",
        TransferPathPolicy.NotificationTapTarget.OPEN_DOWNLOADS,
        decision.target,
      )
      assertNull(decision.mime)
    }

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
}

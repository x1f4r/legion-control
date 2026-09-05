package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.net.*
import org.junit.Assert.*
import org.junit.Test

class UpdateCheckScheduleTest {
    @Test fun `launch checks immediately and quick foreground switches do not repeat requests`() {
        val schedule = UpdateCheckSchedule()
        assertTrue(schedule.due("owner/repo", 0, UpdateCheckSchedule.FOREGROUND_INTERVAL))
        schedule.started("owner/repo", 0)
        assertFalse(schedule.due("owner/repo", 1, UpdateCheckSchedule.FOREGROUND_INTERVAL))
        assertTrue(schedule.due("owner/repo", 15 * 60_000, UpdateCheckSchedule.FOREGROUND_INTERVAL))
    }

    @Test fun `long visible sessions check again and repository changes bypass the interval`() {
        val schedule = UpdateCheckSchedule()
        schedule.started("owner/repo", 100)
        assertFalse(schedule.due("owner/repo", 100 + UpdateCheckSchedule.PERIODIC_INTERVAL - 1, UpdateCheckSchedule.PERIODIC_INTERVAL))
        assertTrue(schedule.due("owner/repo", 100 + UpdateCheckSchedule.PERIODIC_INTERVAL, UpdateCheckSchedule.PERIODIC_INTERVAL))
        assertTrue(schedule.due("other/repo", 101, UpdateCheckSchedule.PERIODIC_INTERVAL))
        assertTrue(schedule.due("owner/repo", 0, UpdateCheckSchedule.PERIODIC_INTERVAL))
    }

    @Test fun `late check and download tickets cannot survive repository changes`() {
        val source = UpdateRepositoryEpoch("owner/first")
        val checkOrigin = source.ticket()
        val downloadOrigin = source.ticket()
        assertTrue(source.matches(checkOrigin, "owner/first"))
        assertFalse("live config guard works before its flow is collected", source.matches(downloadOrigin, "owner/second"))
        assertTrue(source.change("owner/second"))
        assertFalse(source.matches(checkOrigin, "owner/second"))
        val currentOrigin = source.ticket()
        assertTrue(source.matches(currentOrigin, "owner/second"))
        source.change("owner/first")
        assertFalse("changing away and back cannot revive an old APK", source.matches(downloadOrigin, "owner/first"))
        assertFalse(source.matches(currentOrigin, "owner/first"))
    }

    private fun available() = AppUpdates.Check.Available(AppUpdates.Release(
        version = "2.0.0", notes = "", assetUrl = "https://example.invalid/app.apk",
        assetName = ReleaseAssets.ANDROID_APK, sizeBytes = 1,
        manifest = VerifiedManifest(ReleaseManifest(version = "2.0.0"), byteArrayOf()),
    ))

    @Test fun `failed check retains an already verified suggestion only for the same repository`() {
        val previous = available()
        val failed = AppUpdates.Check.Failed("offline")
        assertSame(previous, retainVerifiedUpdate(previous, failed, true))
        assertSame(failed, retainVerifiedUpdate(previous, failed, false))
        assertSame(failed, retainVerifiedUpdate(null, failed, true))
        val current = AppUpdates.Check.UpToDate()
        assertSame(current, retainVerifiedUpdate(previous, current, true))
    }
}

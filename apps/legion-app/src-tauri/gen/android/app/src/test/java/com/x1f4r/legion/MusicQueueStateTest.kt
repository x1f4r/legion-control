package com.x1f4r.legion

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class MusicQueueStateTest {
  private val tracks = listOf(
    MusicQueueState.TrackItem("/music/01.mp3", "01.mp3"),
    MusicQueueState.TrackItem("/music/02.mp3", "02.mp3"),
    MusicQueueState.TrackItem("/music/03.mp3", "03.mp3"),
  )

  @Test
  fun repeatOffPrevAndNextRespectBoundsAnd3SecondRestartRule() {
    val queue = MusicQueueState()
    queue.load("tower", tracks, startIndex = 0, shuffle = false, repeat = MusicQueueState.RepeatMode.OFF)

    assertEquals("/music/01.mp3", queue.currentTrack()?.path)
    assertFalse(queue.hasPrev())
    assertTrue(queue.hasNext())
    assertEquals(1, queue.nextIndex())
    assertEquals(MusicQueueState.PrevDecision.Restart, queue.resolvePrevAction(1_000L))

    queue.moveTo(1)
    assertTrue(queue.hasPrev())
    assertTrue(queue.hasNext())
    assertEquals(MusicQueueState.PrevDecision.Previous(0), queue.resolvePrevAction(3_000L))
    assertEquals(MusicQueueState.PrevDecision.Restart, queue.resolvePrevAction(3_001L))
    assertEquals(MusicQueueState.EndedDecision.Advance(2), queue.resolveEndedAction())

    queue.moveTo(2)
    assertFalse(queue.hasNext())
    assertNull(queue.nextIndex())
    assertEquals(MusicQueueState.EndedDecision.Stop, queue.resolveEndedAction())
  }

  @Test
  fun repeatFolderWrapsBothEndsAndRepeatOneRepeatsCurrent() {
    val queue = MusicQueueState()
    queue.load("tower", tracks, startIndex = 2, shuffle = false, repeat = MusicQueueState.RepeatMode.FOLDER)

    assertTrue(queue.hasNext())
    assertEquals(0, queue.nextIndex())
    assertEquals(MusicQueueState.EndedDecision.Advance(0), queue.resolveEndedAction())

    queue.moveTo(0)
    assertTrue(queue.hasPrev())
    assertEquals(2, queue.prevIndex())
    assertEquals(MusicQueueState.PrevDecision.Previous(2), queue.resolvePrevAction(500L))

    queue.setModes(shuffle = false, repeat = MusicQueueState.RepeatMode.ONE, newOrder = null)
    assertEquals(MusicQueueState.EndedDecision.RepeatCurrent, queue.resolveEndedAction())
    assertTrue(queue.hasNext())
    assertEquals(1, queue.nextIndex())
  }

  @Test
  fun singleTrackFolderHasNoNextAndRepeatsUnderFolderOrOne() {
    val queue = MusicQueueState()
    queue.load("tower", listOf(tracks[0]), startIndex = 0, shuffle = false, repeat = MusicQueueState.RepeatMode.FOLDER)

    assertFalse(queue.hasNext())
    assertFalse(queue.hasPrev())
    assertEquals(MusicQueueState.EndedDecision.RepeatCurrent, queue.resolveEndedAction())

    queue.setModes(shuffle = false, repeat = MusicQueueState.RepeatMode.OFF, newOrder = null)
    assertEquals(MusicQueueState.EndedDecision.Stop, queue.resolveEndedAction())
  }

  @Test
  fun setModesReordersQueueWhileKeepingCurrentTrackSelected() {
    val queue = MusicQueueState()
    queue.load("tower", tracks, startIndex = 1, shuffle = false, repeat = MusicQueueState.RepeatMode.OFF)
    assertEquals("/music/02.mp3", queue.currentTrack()?.path)

    queue.setModes(
      shuffle = true,
      repeat = MusicQueueState.RepeatMode.OFF,
      newOrder = listOf("/music/02.mp3", "/music/03.mp3", "/music/01.mp3"),
    )
    assertEquals(0, queue.currentIndex)
    assertEquals("/music/02.mp3", queue.currentTrack()?.path)
    assertEquals(1, queue.nextIndex())
    assertEquals("/music/03.mp3", queue.trackAt(1)?.path)

    queue.moveTo(1)
    assertEquals("/music/03.mp3", queue.currentTrack()?.path)
    queue.setModes(
      shuffle = false,
      repeat = MusicQueueState.RepeatMode.OFF,
      newOrder = listOf("/music/01.mp3", "/music/02.mp3", "/music/03.mp3"),
    )
    assertEquals(2, queue.currentIndex)
    assertEquals("/music/03.mp3", queue.currentTrack()?.path)
  }

  @Test
  fun formatTitleAndArtistFallsBackToExtensionlessNameAndNeverInventsArtist() {
    assertEquals(
      Pair("Dawn", "Clara"),
      MusicQueueState.formatTitleAndArtist("Dawn", "Clara", "01-dawn.flac"),
    )
    assertEquals(
      Pair("01-dawn", ""),
      MusicQueueState.formatTitleAndArtist(null, null, "01-dawn.flac"),
    )
    assertEquals(".hidden", MusicQueueState.stripFileExtension(".hidden"))
  }
}

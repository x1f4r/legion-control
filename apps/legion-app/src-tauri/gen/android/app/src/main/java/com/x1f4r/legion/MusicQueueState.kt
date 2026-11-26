package com.x1f4r.legion

/**
 * Pure folder-queue, shuffle, repeat, and track display state machine shared by
 * [MusicPlaybackService] and unit tests. Mirrors `apps/ui/lib/player.js` so Android lock-screen,
 * notification, headset, and auto-advance controls obey the exact same rules as the UI.
 */
class MusicQueueState {
  data class TrackItem(
    val path: String,
    val name: String,
    val size: Long? = null,
    val mtime: String? = null,
    val mime: String? = null,
    val durationS: Double? = null,
  )

  enum class RepeatMode(val wireName: String) {
    OFF("off"),
    FOLDER("folder"),
    ONE("one");

    companion object {
      fun fromString(raw: String?): RepeatMode = when (raw?.trim()?.lowercase()) {
        "folder" -> FOLDER
        "one" -> ONE
        else -> OFF
      }
    }
  }

  sealed class PrevDecision {
    object Restart : PrevDecision()
    data class Previous(val index: Int) : PrevDecision()
  }

  sealed class EndedDecision {
    object Stop : EndedDecision()
    object RepeatCurrent : EndedDecision()
    data class Advance(val index: Int) : EndedDecision()
  }

  var machine: String = ""
    private set
  var shuffle: Boolean = false
    private set
  var repeat: RepeatMode = RepeatMode.OFF
    private set
  var currentIndex: Int = -1
    private set

  private val tracksByPath = LinkedHashMap<String, TrackItem>()
  private var order: List<String> = emptyList()

  val isEmpty: Boolean
    get() = order.isEmpty() || currentIndex !in order.indices

  val size: Int
    get() = order.size

  fun load(
    machine: String,
    tracks: List<TrackItem>,
    startIndex: Int,
    shuffle: Boolean,
    repeat: RepeatMode,
  ): TrackItem? {
    this.machine = machine.trim()
    this.shuffle = shuffle
    this.repeat = repeat
    tracksByPath.clear()
    val validOrder = ArrayList<String>(tracks.size)
    for (item in tracks) {
      if (item.path.isNotEmpty() && !tracksByPath.containsKey(item.path)) {
        tracksByPath[item.path] = item
        validOrder.add(item.path)
      }
    }
    order = validOrder
    currentIndex = if (order.isEmpty()) -1 else startIndex.coerceIn(0, order.lastIndex)
    return currentTrack()
  }

  fun clear() {
    machine = ""
    shuffle = false
    repeat = RepeatMode.OFF
    currentIndex = -1
    tracksByPath.clear()
    order = emptyList()
  }

  fun setModes(shuffle: Boolean, repeat: RepeatMode, newOrder: List<String>?) {
    val activePath = currentTrack()?.path
    this.shuffle = shuffle
    this.repeat = repeat
    if (!newOrder.isNullOrEmpty() && tracksByPath.isNotEmpty()) {
      val seen = HashSet<String>(newOrder.size)
      val rebuilt = ArrayList<String>(tracksByPath.size)
      for (p in newOrder) {
        if (tracksByPath.containsKey(p) && seen.add(p)) {
          rebuilt.add(p)
        }
      }
      // Keep any tracks in the folder that were not listed in `newOrder` at the tail.
      for (p in tracksByPath.keys) {
        if (seen.add(p)) {
          rebuilt.add(p)
        }
      }
      if (rebuilt.isNotEmpty()) {
        order = rebuilt
      }
    }
    if (activePath != null) {
      val idx = order.indexOf(activePath)
      if (idx >= 0) {
        currentIndex = idx
      }
    }
  }

  fun currentTrack(): TrackItem? = order.getOrNull(currentIndex)?.let { tracksByPath[it] }

  fun trackAt(index: Int): TrackItem? = order.getOrNull(index)?.let { tracksByPath[it] }

  fun allTracksInOrder(): List<TrackItem> = order.mapNotNull { tracksByPath[it] }

  fun currentOrderPaths(): List<String> = order

  fun prevIndex(): Int? {
    if (order.size <= 1 || currentIndex !in order.indices) return null
    if (currentIndex > 0) return currentIndex - 1
    return if (repeat == RepeatMode.FOLDER) order.lastIndex else null
  }

  fun nextIndex(): Int? {
    if (order.size <= 1 || currentIndex !in order.indices) return null
    if (currentIndex + 1 < order.size) return currentIndex + 1
    return if (repeat == RepeatMode.FOLDER) 0 else null
  }

  fun hasPrev(): Boolean = prevIndex() != null

  fun hasNext(): Boolean = nextIndex() != null

  fun moveTo(index: Int): TrackItem? {
    if (index !in order.indices) return null
    currentIndex = index
    return currentTrack()
  }

  fun resolvePrevAction(positionMs: Long): PrevDecision {
    if (positionMs > PREV_RESTART_THRESHOLD_MS) {
      return PrevDecision.Restart
    }
    val prev = prevIndex() ?: return PrevDecision.Restart
    return PrevDecision.Previous(prev)
  }

  fun resolveEndedAction(): EndedDecision {
    if (isEmpty) return EndedDecision.Stop
    if (repeat == RepeatMode.ONE) return EndedDecision.RepeatCurrent
    val next = nextIndex()
    if (next != null) return EndedDecision.Advance(next)
    if (repeat == RepeatMode.FOLDER && order.size == 1) return EndedDecision.RepeatCurrent
    return EndedDecision.Stop
  }

  companion object {
    const val PREV_RESTART_THRESHOLD_MS = 3_000L

    fun stripFileExtension(name: String): String {
      val trimmed = name.trim()
      val dot = trimmed.lastIndexOf('.')
      if (dot <= 0) return trimmed
      return trimmed.substring(0, dot)
    }

    fun formatTitleAndArtist(
      tagTitle: String?,
      tagArtist: String?,
      fileName: String,
    ): Pair<String, String> {
      val cleanTitle = tagTitle?.trim().orEmpty().ifEmpty { stripFileExtension(fileName) }
      val cleanArtist = tagArtist?.trim().orEmpty()
      return Pair(cleanTitle, cleanArtist)
    }
  }
}

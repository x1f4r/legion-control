package com.x1f4r.legioncontrol.data

import android.content.SharedPreferences

/**
 * Preferences in a map, for the stores that keep their state in them.
 *
 * Every apply or commit is remembered as the set of keys it wrote, because what some of these tests
 * are about is which values went to disk together: a document and the approvals it carries must never
 * be two writes with a moment between them.
 */
class MemoryPreferences : SharedPreferences {
    val values = linkedMapOf<String, Any?>()

    /** The keys of each write, in the order they were applied. */
    val writes = mutableListOf<Set<String>>()

    /**
     * While set, every commit fails the way it fails on Android when the disk will not take it: the
     * map in memory has the new values all the same, and the commit returns false.
     */
    var diskRefuses = false

    /**
     * Runs once, inside the next commit and after it reached the map: a write from elsewhere in the
     * process that lands while that commit is still on its way to the disk.
     */
    var duringNextCommit: (() -> Unit)? = null

    override fun getAll(): MutableMap<String, *> = values.toMutableMap()

    override fun getString(key: String?, defValue: String?): String? =
        if (values.containsKey(key)) values[key] as String? else defValue

    @Suppress("UNCHECKED_CAST")
    override fun getStringSet(key: String?, defValues: MutableSet<String>?): MutableSet<String>? =
        if (values.containsKey(key)) values[key] as MutableSet<String>? else defValues

    override fun getInt(key: String?, defValue: Int): Int = values[key] as Int? ?: defValue

    override fun getLong(key: String?, defValue: Long): Long = values[key] as Long? ?: defValue

    override fun getFloat(key: String?, defValue: Float): Float = values[key] as Float? ?: defValue

    override fun getBoolean(key: String?, defValue: Boolean): Boolean = values[key] as Boolean? ?: defValue

    override fun contains(key: String?): Boolean = values.containsKey(key)

    override fun edit(): SharedPreferences.Editor = Editor()

    override fun registerOnSharedPreferenceChangeListener(listener: SharedPreferences.OnSharedPreferenceChangeListener?) = Unit

    override fun unregisterOnSharedPreferenceChangeListener(listener: SharedPreferences.OnSharedPreferenceChangeListener?) = Unit

    private inner class Editor : SharedPreferences.Editor {
        private val pending = linkedMapOf<String, Any?>()
        private val removed = mutableSetOf<String>()
        private var cleared = false

        private fun put(key: String?, value: Any?): SharedPreferences.Editor = apply {
            val name = key ?: return@apply
            // A null value removes the key, as it does on Android.
            if (value == null) {
                pending.remove(name)
                removed += name
            } else {
                pending[name] = value
            }
        }

        override fun putString(key: String?, value: String?) = put(key, value)

        override fun putStringSet(key: String?, values: MutableSet<String>?) = put(key, values?.toMutableSet())

        override fun putInt(key: String?, value: Int) = put(key, value)

        override fun putLong(key: String?, value: Long) = put(key, value)

        override fun putFloat(key: String?, value: Float) = put(key, value)

        override fun putBoolean(key: String?, value: Boolean) = put(key, value)

        override fun remove(key: String?): SharedPreferences.Editor = apply { key?.let(removed::add) }

        override fun clear(): SharedPreferences.Editor = apply { cleared = true }

        override fun commit(): Boolean {
            if (cleared) values.clear()
            removed.forEach { values.remove(it) }
            values.putAll(pending)
            writes += pending.keys + removed
            duringNextCommit?.let {
                duringNextCommit = null
                it()
            }
            return !diskRefuses
        }

        override fun apply() {
            commit()
        }
    }
}

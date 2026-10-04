package com.x1f4r.legioncontrol.ui

import android.util.Log
import kotlinx.coroutines.CoroutineExceptionHandler

/**
 * The last word on a failure nothing else caught, for the scopes that outlive a screen.
 *
 * Every call to a machine already turns what can go wrong into an AgentFailure, and that into a
 * sentence. What is left is a defect in this app. A scope with a SupervisorJob and no handler hands
 * one of those to the thread, and on Android an uncaught exception ends the process, taking the
 * bookkeeping of every change still in flight with it. Here it is logged instead, with its stack,
 * and the scope carries on with everything else it was doing.
 */
internal fun uncaughtFailureHandler(
    tag: String,
    report: (Throwable) -> Unit = { failure -> Log.e(tag, "A coroutine failed and nothing caught it.", failure) },
): CoroutineExceptionHandler = CoroutineExceptionHandler { _, failure -> report(failure) }

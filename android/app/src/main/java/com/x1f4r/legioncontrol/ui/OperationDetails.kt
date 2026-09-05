package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.OpRecord
import com.x1f4r.legioncontrol.agent.OperationRecord

/** Separate fields keep a short result message from hiding an action's retained output. */
fun operationResultDetails(message: String?, output: String?): String? = listOfNotNull(
    message?.takeIf { it.isNotBlank() }?.let { "Message\n$it" },
    output?.takeIf { it.isNotBlank() }?.let { "Output\n$it" },
).takeIf { it.isNotEmpty() }?.joinToString("\n\n")

/** Human-readable details remain useful from local history while a machine is unreachable. */
fun operationDetails(local: OperationRecord?, remote: OpRecord? = null): String = buildList {
    local?.summary?.takeIf { it.isNotBlank() }?.let(::add)
    listOfNotNull(
        remote?.kind?.let { "Change: $it" },
        remote?.subject?.let { "Target: $it" },
        remote?.state?.let { "State: $it" },
        remote?.phase?.let { "Phase: $it" },
        remote?.result?.action?.let { "Result: $it" },
        remote?.result?.reasonCode?.let { "Reason: $it" },
        remote?.expiresAt?.let { "Expires: $it" },
    ).takeIf { it.isNotEmpty() }?.joinToString("\n")?.let(::add)
    operationResultDetails(
        remote?.result?.message?.takeIf { it.isNotBlank() } ?: local?.detail,
        remote?.result?.output?.takeIf { it.isNotBlank() } ?: local?.output,
    )?.let(::add)
    val log = (local?.log.orEmpty().map { it.text } + remote?.log.orEmpty().mapNotNull { it.line })
        .filter { it.isNotBlank() }.distinct()
    if (log.isNotEmpty()) add("Log\n${log.joinToString("\n")}")
    (remote?.id ?: local?.id)?.let { add("Operation: $it") }
}.joinToString("\n\n")

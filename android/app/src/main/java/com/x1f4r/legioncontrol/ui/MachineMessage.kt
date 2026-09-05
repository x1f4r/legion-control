package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.OperationRecord

/** The latest activity message, optionally attached to a durable operation's current outcome. */
data class MachineMessage(
    val line: String = "Ready.",
    val isError: Boolean = false,
    val detail: String? = null,
    val operationId: String? = null,
) {
    fun resolve(records: List<OperationRecord>): MachineMessage =
        records.firstOrNull { it.id == operationId }?.let(::forOperation) ?: this

    companion object {
        fun forOperation(record: OperationRecord) = MachineMessage(
            line = record.summary,
            isError = record.succeeded == false,
            detail = operationResultDetails(record.detail, record.output),
            operationId = record.id,
        )
    }
}

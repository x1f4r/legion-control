package com.x1f4r.legioncontrol.ui

import com.x1f4r.legioncontrol.agent.*
import org.junit.Assert.*
import org.junit.Test

class OperationDetailsTest {
    private fun local() = OperationRecord(
        id = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0", machineId = "pi", machineName = "Pi",
        intent = OperationIntent(OpKind.RUN, actionId = "count"), subjectName = "Count", initiator = "Phone",
        startedAt = 1, updatedAt = 2, state = TrackedState.FINISHED, summary = "Count ran.",
        detail = "Action completed", output = "count=7\nready", log = listOf(OperationLogLine(1, "Started Count")),
    )

    @Test fun `message output and log remain distinct in saved details`() {
        val rendered = operationDetails(local())
        assertTrue(rendered.contains("Message\nAction completed"))
        assertTrue(rendered.contains("Output\ncount=7\nready"))
        assertTrue(rendered.contains("Log\nStarted Count"))
        val footer = MachineMessage.forOperation(local()).detail.orEmpty()
        assertTrue(footer.contains("Action completed"))
        assertTrue(footer.contains("count=7"))
    }

    @Test fun `remote message without output preserves locally recorded action output`() {
        val remote = OpRecord(id = local().id, state = "finished", result = OpResult(action = "ran", message = "Verified"))
        val rendered = operationDetails(local(), remote)
        assertTrue(rendered.contains("Message\nVerified"))
        assertTrue(rendered.contains("Output\ncount=7\nready"))
    }

    @Test fun `peer record includes both result message and output`() {
        val remote = OpRecord(id = "peer", state = "finished", result = OpResult(action = "ran", message = "Done", output = "stdout retained"))
        val rendered = operationDetails(null, remote)
        assertTrue(rendered.contains("Message\nDone"))
        assertTrue(rendered.contains("Output\nstdout retained"))
    }
}

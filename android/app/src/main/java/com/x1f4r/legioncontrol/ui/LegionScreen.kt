package com.x1f4r.legioncontrol.ui

import androidx.compose.animation.Crossfade
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DrawerValue
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalDrawerSheet
import androidx.compose.material3.ModalNavigationDrawer
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.rememberDrawerState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.RectangleShape
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.x1f4r.legioncontrol.ui.theme.LocalStatusColors
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.launch

/**
 * A page per subject rather than one long scroll.
 *
 * The split is by subject and by who owns it: each machine, each service running on it, the shared
 * setup, and this phone. Both ways of moving between them are real. The drawer is for going
 * somewhere by name; the swipe is for the next page being one gesture away, which is how siblings
 * behave on a phone. They are the same state, so neither can be out of step with the other.
 */
sealed interface Page {
    val title: String
    val summary: String

    data class OneMachine(val model: MachineModel) : Page {
        override val title get() = model.machine.name
        override val summary get() = "Which system is up, what it is doing, and the power actions"
    }

    data class OneService(
        val model: MachineModel,
        val service: RememberedService,
        val showsMachine: Boolean,
    ) : Page {
        override val title get() = service.name
        override val summary
            get() = if (showsMachine) {
                "On ${model.machine.name}: versions, health, and when it may update"
            } else {
                "Versions, health, and when it may update"
            }
    }

    /** What the app is before it has been told about anything. */
    data object NoMachines : Page {
        override val title get() = "Machines"
        override val summary get() = "Fetch the setup from one of them to begin"
    }

    /** Two copies of the same setup, waiting for somebody to settle them. */
    data object Divergence : Page {
        override val title get() = "Setup edited twice"
        override val summary get() = "Two copies that neither made from the other"
    }

    /** A machine belonging to a different setup entirely. */
    data object IdentityClash : Page {
        override val title get() = "A different setup"
        override val summary get() = "One machine belongs to another setup"
    }

    data object Editor : Page {
        override val title get() = "Edit the setup"
        override val summary get() = "Machines, addresses, systems, sites and wake helpers"
    }

    data object Device : Page {
        override val title get() = "This device"
        override val summary get() = "The app, this phone's key, and the setup it shares"
    }
}

/** The pages there are right now, which follows the setup and the services it found. */
private fun pagesOf(app: AppModel): List<Page> {
    val machines = app.machines
    return buildList {
        // A question the app cannot answer on its own goes first, because it is holding everything
        // else up: nothing is adopted or published while one is outstanding.
        if (app.divergence != null) add(Page.Divergence)
        if (app.identityClash != null) add(Page.IdentityClash)
        if (machines.isEmpty()) add(Page.NoMachines)
        machines.forEach { model ->
            add(Page.OneMachine(model))
            model.knownServices.forEach { service ->
                add(Page.OneService(model, service, showsMachine = machines.size > 1))
            }
        }
        if (app.editorOpen) add(Page.Editor)
        add(Page.Device)
    }
}

@Composable
fun LegionApp(app: AppModel, updates: AppUpdateModel) {
    Surface(
        modifier = Modifier.fillMaxSize(),
        color = MaterialTheme.colorScheme.background,
    ) {
        if (app.needsKeyAuthorisation) {
            AuthorisationScreen(
                publicKey = app.publicKey,
                systems = app.machines.flatMap { it.machine.systems },
                detail = app.keyAuthorisationDetail,
                onRetry = { app.refreshAll() },
            )
        } else {
            LegionScreen(app, updates)
        }
    }
}

@Composable
private fun LegionScreen(app: AppModel, updates: AppUpdateModel) {
    val pages = pagesOf(app)
    val pager = rememberPagerState(pageCount = { pages.size })
    val drawer = rememberDrawerState(DrawerValue.Closed)
    val scope = rememberCoroutineScope()
    val haptics = rememberHaptics()
    val bindings by app.deviceBindings.collectAsState()

    LaunchedEffect(pager, haptics) {
        snapshotFlow { pager.settledPage }
            .distinctUntilChanged()
            .drop(1)
            .collect { haptics.tick() }
    }

    LaunchedEffect(pager, pages) {
        snapshotFlow { pager.currentPage }
            .distinctUntilChanged()
            .collect { index ->
                app.currentPage = index
                app.showing(
                    when (val page = pages.getOrNull(index)) {
                        is Page.OneMachine -> page.model
                        is Page.OneService -> page.model
                        else -> null
                    },
                )
            }
    }

    LaunchedEffect(updates) { updates.start() }

    val currentPage = { pager.currentPage }

    ModalNavigationDrawer(
        drawerState = drawer,
        drawerContent = {
            PageDrawer(
                pages = pages,
                selected = currentPage,
                onSelect = { index ->
                    scope.launch {
                        drawer.close()
                        pager.animateScrollToPage(index)
                    }
                },
            )
        },
    ) {
        Scaffold(
            containerColor = MaterialTheme.colorScheme.background,
            topBar = {
                PageBar(
                    pages = pages,
                    current = currentPage,
                    onMenu = { scope.launch { drawer.open() } },
                )
            },
            bottomBar = { StatusFooter(app) },
        ) { insets ->
            Refreshable(
                isRefreshing = app.footerMachine?.isRefreshingVisibly == true,
                onRefresh = {
                    app.refreshAll()
                    updates.recheck()
                },
                modifier = Modifier
                    .fillMaxSize()
                    .padding(insets),
            ) {
                HorizontalPager(state = pager, modifier = Modifier.fillMaxSize()) { index ->
                    Column(
                        modifier = Modifier
                            .fillMaxSize()
                            .verticalScroll(rememberScrollState())
                            .padding(horizontal = 20.dp)
                            .padding(top = 6.dp, bottom = 28.dp),
                    ) {
                        when (val page = pages.getOrNull(index)) {
                            is Page.OneMachine -> MachineSection(page.model, app)
                            is Page.OneService -> ServiceSection(page.model, page.service)
                            Page.NoMachines -> NoMachinesSection(app)
                            Page.Divergence -> DivergenceSection(app)
                            Page.IdentityClash -> IdentityClashSection(app)
                            Page.Editor -> SetupEditorSection(app)
                            Page.Device, null ->
                                ThisDeviceSection(app, updates, bindings, app.bindingsActions)
                        }
                    }
                }
            }
        }
    }

    app.fetchDialog?.let { dialog ->
        HostTrustDialog(
            dialog = dialog,
            onDismiss = { app.fetchDialog = null },
            onConfirm = { approval ->
                app.fetchDialog = null
                haptics.confirm()
                app.trustFetchHostKey(dialog.address, dialog.keyBlob, approval)
            },
        )
    }

    app.dialogOwner?.let { model ->
        model.dialog?.let { dialog ->
            if (dialog is MachineModel.Dialog.ConfirmTrustHostKey) {
                HostTrustDialog(dialog, onDismiss = { model.dialog = null }, onConfirm = { approval ->
                    model.dialog = null
                    model.trustHostKey(dialog.address, dialog.keyBlob, approval)
                })
            } else ConfirmationDialog(
                dialog = dialog,
                onDismiss = { model.dialog = null },
                onConfirm = {
                    model.dialog = null
                    // The firmer haptic belongs here rather than on the button that opened the
                    // question. This is the press that actually interrupts the machine.
                    haptics.confirm()
                    perform(model, dialog, whenIdle = false)
                },
                onAlternative = dialog.alternativeLabel?.let {
                    {
                        model.dialog = null
                        haptics.tick()
                        perform(model, dialog, whenIdle = true)
                    }
                },
            )
        }
    }
}

/**
 * What a confirmed question actually does.
 *
 * Never forced from the first question, however busy the last reading looked: force is the agent's
 * own check being skipped, and it is only ever offered back after the agent has looked at the
 * machine as it is now and said no. [whenIdle] is the quieter answer to the same question, which
 * queues the change rather than interrupting anything.
 */
private fun perform(model: MachineModel, dialog: MachineModel.Dialog, whenIdle: Boolean) {
    when (dialog) {
        is MachineModel.Dialog.ConfirmBoot -> model.boot(dialog.target, whenIdle = whenIdle)
        is MachineModel.Dialog.OfferForceBoot -> model.boot(dialog.target, force = true)
        is MachineModel.Dialog.ConfirmSleep -> model.sleep(whenIdle = whenIdle)
        is MachineModel.Dialog.OfferForceSleep -> model.sleep(force = true)
        is MachineModel.Dialog.ConfirmRestart -> model.restart(dialog.service, whenIdle = whenIdle)
        is MachineModel.Dialog.OfferForceRestart -> model.restart(dialog.service, force = true)
        is MachineModel.Dialog.OfferForceUpdate -> model.update(dialog.service, force = true)
        is MachineModel.Dialog.ConfirmRun -> model.runAction(dialog.action, whenIdle = whenIdle)
        is MachineModel.Dialog.OfferForceRun -> model.runAction(dialog.action, force = true)
        is MachineModel.Dialog.ConfirmTrustHostKey -> Unit

        is MachineModel.Dialog.ConfirmAgentInstall -> model.installAgent()
        is MachineModel.Dialog.ConfirmWakeHelper -> model.wakeHelper()
    }
}

/**
 * What there is to say, and to do, before anything has been configured.
 *
 * The way in rather than a sign pointing at one. Every machine carries the setup, so the first
 * launch needs one address and not a document.
 */
@Composable
private fun NoMachinesSection(app: AppModel) {
    Spacer(Modifier.height(6.dp))
    ExplanationText(
        "No machines yet. Each of them carries the setup, so one address, its port and the user to " +
            "log in as is enough to fetch all of it.",
    )
    Spacer(Modifier.height(14.dp))
    FetchSetupBlock(app)
}

// MARK: - Chrome

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun PageBar(pages: List<Page>, current: () -> Int, onMenu: () -> Unit) {
    Column {
        TopAppBar(
            title = {
                Crossfade(
                    targetState = pages.getOrNull(current())?.title.orEmpty(),
                    animationSpec = tween(180),
                    label = "title",
                ) { title ->
                    Text(title, style = MaterialTheme.typography.titleSmall)
                }
            },
            navigationIcon = { HamburgerButton(onMenu) },
            colors = TopAppBarDefaults.topAppBarColors(
                containerColor = MaterialTheme.colorScheme.background,
                titleContentColor = MaterialTheme.colorScheme.onSurface,
            ),
        )
        PageRule(pages.size, current)
    }
}

/**
 * Three lines, drawn rather than imported.
 *
 * The rest of this app draws its own marks and glyphs, at one weight, from theme colours. One icon
 * pulled out of a set here would be the only shape on screen tuned by somebody else.
 */
@Composable
private fun HamburgerButton(onClick: () -> Unit) {
    val colour = MaterialTheme.colorScheme.onSurface
    IconButton(onClick = onClick) {
        Canvas(
            Modifier
                .size(19.dp)
                .semantics { contentDescription = "Pages" },
        ) {
            val stroke = size.minDimension * 0.095f
            listOf(0.2f, 0.5f, 0.8f).forEach { fraction ->
                drawLine(
                    color = colour,
                    start = Offset(0f, size.height * fraction),
                    end = Offset(size.width, size.height * fraction),
                    strokeWidth = stroke,
                    cap = StrokeCap.Round,
                )
            }
        }
    }
}

/**
 * Where you are, as segments of one rule.
 *
 * Not dots, and not a tab bar: the page already says its name in the bar above, so this only has to
 * say which part of the app it is, and a rule is the one device this page already uses to divide
 * things.
 */
@Composable
private fun PageRule(count: Int, current: () -> Int) {
    val active = MaterialTheme.colorScheme.onSurface
    val idle = MaterialTheme.colorScheme.outlineVariant
    val page = current()
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 20.dp)
            .height(2.dp),
    ) {
        repeat(count) { index ->
            val colour by animateColorAsState(
                targetValue = if (index == page) active else idle,
                animationSpec = tween(200),
                label = "segment",
            )
            Box(
                Modifier
                    .weight(1f)
                    .fillMaxHeight()
                    .background(colour),
            )
            if (index < count - 1) Spacer(Modifier.width(6.dp))
        }
    }
}

@Composable
private fun PageDrawer(
    pages: List<Page>,
    selected: () -> Int,
    onSelect: (Int) -> Unit,
) {
    val current = selected()
    ModalDrawerSheet(
        drawerShape = RectangleShape,
        drawerContainerColor = MaterialTheme.colorScheme.background,
        drawerContentColor = MaterialTheme.colorScheme.onSurface,
        modifier = Modifier.width(292.dp),
    ) {
        Column(
            Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .systemBarsPadding()
                .padding(horizontal = 20.dp),
        ) {
            Spacer(Modifier.height(22.dp))
            Text("Legion Control", style = MaterialTheme.typography.titleLarge)
            Spacer(Modifier.height(4.dp))
            QuietText("Your machines, what runs on them, and this phone.")
            Spacer(Modifier.height(20.dp))
            HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
            pages.forEachIndexed { index, page ->
                DrawerRow(page, index == current) { onSelect(index) }
                HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
            }
        }
    }
}

/**
 * A name and a line about it, on a row a thumb can hit. What marks the current one is a short rule
 * on the leading edge and the ink getting darker, not a filled capsule behind the text.
 */
@Composable
private fun DrawerRow(page: Page, isSelected: Boolean, onSelect: () -> Unit) {
    val marker by animateColorAsState(
        targetValue = if (isSelected) {
            MaterialTheme.colorScheme.primary
        } else {
            MaterialTheme.colorScheme.background
        },
        animationSpec = tween(180),
        label = "marker",
    )
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .defaultMinSize(minHeight = 68.dp)
            .clickable { onSelect() }
            .padding(vertical = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            Modifier
                .width(2.dp)
                .height(30.dp)
                .background(marker),
        )
        Spacer(Modifier.width(14.dp))
        Column(Modifier.weight(1f)) {
            Text(
                text = page.title,
                style = MaterialTheme.typography.bodyLarge,
                fontWeight = if (isSelected) FontWeight.SemiBold else FontWeight.Normal,
                color = if (isSelected) {
                    MaterialTheme.colorScheme.onSurface
                } else {
                    MaterialTheme.colorScheme.onSurfaceVariant
                },
            )
            QuietText(page.summary, Modifier.padding(top = 2.dp))
        }
    }
}

// MARK: - Footer

/**
 * The one line that belongs to every page, so it sits under all of them rather than inside any of
 * them. What the app is doing right now is not a property of the page you happen to be on.
 */
@Composable
private fun StatusFooter(app: AppModel) {
    val palette = LocalStatusColors.current
    val model = app.footerMachine
    Column(Modifier.fillMaxWidth()) {
        HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .navigationBarsPadding()
                .padding(horizontal = 20.dp, vertical = 12.dp),
            verticalAlignment = Alignment.Top,
        ) {
            Box(Modifier.padding(top = 5.dp).size(11.dp), contentAlignment = Alignment.Center) {
                if (model != null && (model.isWorking || model.isRefreshingVisibly)) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(11.dp),
                        strokeWidth = 1.5.dp,
                        color = palette.quiet,
                    )
                } else {
                    StatusMark(
                        when {
                            model?.statusIsError == true -> Mark.Attention
                            model?.isUnsettled == true -> Mark.Unknown
                            else -> Mark.Idle
                        },
                    )
                }
            }
            Spacer(Modifier.width(9.dp))
            Column(Modifier.weight(1f)) {
                Crossfade(
                    targetState = model?.statusLine ?: "Nothing is configured yet.",
                    animationSpec = tween(190),
                    label = "statusLine",
                ) { line ->
                    Text(line, style = MaterialTheme.typography.bodyMedium)
                }
                if (model != null) {
                    QuietText(routeAndAge(model, app.now), Modifier.padding(top = 2.dp))
                }
                model?.statusDetail?.takeIf { it.isNotBlank() }?.let { detail ->
                    Text(
                        text = detail,
                        style = MaterialTheme.typography.bodySmall
                            .copy(fontFamily = FontFamily.Monospace),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 4,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                }
            }
        }
    }
}

/**
 * Which way the reading came in and how old it is.
 *
 * The route is only claimed in the present tense while the link is up. A remembered route under a
 * failed reading would name a connection that is not there, which is the one thing this line exists
 * to avoid.
 */
private fun routeAndAge(model: MachineModel, now: Long): String {
    val age = freshness(model.lastChecked, now)
    val route = model.route
    return when {
        model.link is LinkState.Online && route != null -> "${route.displayName}, $age"
        route != null -> "last answered ${route.displayName}, $age"
        else -> age
    }
}

// MARK: - Dialogs

@Composable
private fun ConfirmationDialog(
    dialog: MachineModel.Dialog,
    onDismiss: () -> Unit,
    onConfirm: () -> Unit,
    onAlternative: (() -> Unit)? = null,
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(dialog.title) },
        text = { Text(dialog.message) },
        confirmButton = {
            Row {
                // The quieter answer to the same question: do it, but wait until nothing is running.
                if (onAlternative != null) {
                    TextButton(onClick = onAlternative) {
                        Text(dialog.alternativeLabel.orEmpty())
                    }
                }
                TextButton(onClick = onConfirm) {
                    Text(dialog.confirmLabel, color = LocalStatusColors.current.bad)
                }
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        },
    )
}

/** Reads a flow into composition without dragging a lifecycle dependency into every call site. */
@Composable
internal fun <T> StateFlow<T>.collectAsStateSafely(): T {
    val state: State<T> = collectAsState()
    return state.value
}

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
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.launch

/**
 * A page per subject rather than one long scroll.
 *
 * The old page put a machine's power state and a wall of base64 at the same level, one under the
 * other, so the thing you opened the app for had to be scrolled past the thing you needed once. The
 * split is by subject and by who owns it: each machine, each service running on it, and this phone.
 *
 * Both ways of moving between them are real. The drawer is for going somewhere by name; the swipe is
 * for the next page being one gesture away, which is how siblings behave on a phone. They are the
 * same state, so neither can be out of step with the other.
 */
sealed interface Page {
    val title: String
    val summary: String

    data class OneMachine(val model: MachineModel) : Page {
        override val title get() = model.machine.name
        override val summary get() = "Which system is up, and the power actions"
    }

    data class OneService(
        val model: MachineModel,
        val service: RememberedService,
        /** Only worth saying when there is more than one machine to tell apart. */
        val showsMachine: Boolean,
    ) : Page {
        override val title get() = service.name
        override val summary
            get() = if (showsMachine) {
                "On ${model.machine.name}: versions, health, and what it is doing"
            } else {
                "Versions, health, and what it is doing"
            }
    }

    /** What the app is before it has been told about anything. */
    data object NoMachines : Page {
        override val title get() = "Machines"
        override val summary get() = "Fetch the setup from one of them to begin"
    }

    data object Device : Page {
        override val title get() = "This device"
        override val summary get() = "The app, this phone's key, and the configuration"
    }
}

/** The pages there are right now, which follows the configuration and the services it found. */
private fun pagesOf(app: AppModel): List<Page> {
    val machines = app.machines
    return buildList {
        if (machines.isEmpty()) add(Page.NoMachines)
        machines.forEach { model ->
            add(Page.OneMachine(model))
            model.knownServices.forEach { service ->
                add(Page.OneService(model, service, showsMachine = machines.size > 1))
            }
        }
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

    // The tick belongs to the moment the swipe lands, not to every frame that crosses a boundary,
    // so this watches the settled page rather than the current one. The first emission is the
    // initial page and is dropped: arriving at the app is not a page change.
    LaunchedEffect(pager, haptics) {
        snapshotFlow { pager.settledPage }
            .distinctUntilChanged()
            .drop(1)
            .collect { haptics.tick() }
    }

    // The footer speaks for the machine whose page is in front, and keeps speaking for it on the
    // pages that belong to no machine.
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

    // Once, at launch, rather than when the page that shows it is first swiped to. Releases are
    // cut by hand every few days at most, so one request per run of the app is the whole budget.
    LaunchedEffect(updates) { updates.start() }

    // Which page is current is read inside the two things that draw it, not out here. Reading it at
    // this level would put the whole scaffold, the drawer and every page through a recomposition
    // every time a swipe crosses a boundary, to move one title and one segment of a rule.
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
                        // Animated rather than snapped, so the drawer and the swipe arrive the same
                        // way and the app has one way of moving instead of two.
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
                HorizontalPager(
                    state = pager,
                    modifier = Modifier.fillMaxSize(),
                ) { index ->
                    Column(
                        modifier = Modifier
                            .fillMaxSize()
                            .verticalScroll(rememberScrollState())
                            .padding(horizontal = 20.dp)
                            .padding(top = 6.dp, bottom = 28.dp),
                    ) {
                        when (val page = pages.getOrNull(index)) {
                            is Page.OneMachine -> MachineSection(page.model)
                            is Page.OneService -> ServiceSection(page.model, page.service)
                            Page.NoMachines -> NoMachinesSection(app)
                            Page.Device, null -> ThisDeviceSection(app, updates)
                        }
                    }
                }
            }
        }
    }

    // The one question this app asks that belongs to no machine, because it is asked about an
    // address that is not in any configuration yet. Same words, same shape, same trust store.
    app.fetchDialog?.let { dialog ->
        ConfirmationDialog(
            dialog = dialog,
            onDismiss = { app.fetchDialog = null },
            onConfirm = {
                app.fetchDialog = null
                haptics.confirm()
                app.trustFetchHostKey(dialog.address, dialog.keyBlob)
            },
        )
    }

    app.dialogOwner?.let { model ->
        model.dialog?.let { dialog ->
            ConfirmationDialog(
                dialog = dialog,
                onDismiss = { model.dialog = null },
                onConfirm = {
                    model.dialog = null
                    // The firmer haptic belongs here rather than on the button that opened the
                    // question. This is the press that actually interrupts the machine.
                    haptics.confirm()
                    when (dialog) {
                        // Never forced from here, however busy the last reading looked. Force is the
                        // agent's own check being skipped, and it is only ever offered back after the
                        // agent has looked at the machine as it is now and said no.
                        is MachineModel.Dialog.ConfirmBoot -> model.boot(dialog.target, force = false)

                        is MachineModel.Dialog.OfferForceBoot -> model.boot(dialog.target, force = true)
                        is MachineModel.Dialog.ConfirmSleep -> model.sleep(force = false)
                        is MachineModel.Dialog.OfferForceSleep -> model.sleep(force = true)
                        is MachineModel.Dialog.ConfirmRestart ->
                            model.restart(dialog.service, force = false)

                        is MachineModel.Dialog.OfferForceRestart ->
                            model.restart(dialog.service, force = true)

                        is MachineModel.Dialog.OfferForceUpdate ->
                            model.update(dialog.service, force = true)

                        is MachineModel.Dialog.ConfirmRun -> model.runAction(dialog.action, force = false)
                        is MachineModel.Dialog.OfferForceRun -> model.runAction(dialog.action, force = true)
                        is MachineModel.Dialog.ConfirmTrustHostKey ->
                            model.trustHostKey(dialog.address, dialog.keyBlob)
                    }
                },
            )
        }
    }
}

/**
 * What there is to say, and to do, before anything has been configured.
 *
 * The way in rather than a sign pointing at one. The machines carry the setup, so the first launch
 * needs one address and not a document, and asking for it here is asking for it where the user
 * already is. Pasting a document by hand is still there, one page along, under This device.
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
            // No haptic on the press. The drawer sliding shut is the acknowledgement, and the
            // page change at the far end of the animation already ticks; two buzzes for one tap
            // is the app being pleased with itself.
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
                // The quiet fifteen second poll deliberately does not spin this. A mark that swaps
                // itself for a spinner four times a minute is movement at the bottom of a page that
                // is otherwise still, and it says nothing the "checked just now" line does not.
                if (model != null && (model.isWorking || model.isRefreshingVisibly)) {
                    CircularProgressIndicator(
                        modifier = Modifier.size(11.dp),
                        strokeWidth = 1.5.dp,
                        color = palette.quiet,
                    )
                } else {
                    StatusMark(if (model?.statusIsError == true) Mark.Attention else Mark.Idle)
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
) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(dialog.title) },
        text = { Text(dialog.message) },
        confirmButton = {
            TextButton(onClick = onConfirm) {
                Text(dialog.confirmLabel, color = LocalStatusColors.current.bad)
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        },
    )
}

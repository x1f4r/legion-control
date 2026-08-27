#Requires -Version 5.1
<#
    Legion Control: install the agent on a Windows system and hand the update
    job over to it.

    Run it over SSH as the account that owns the install:

        ssh <machine> "powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\<you>\legion-control\agent\install\install-windows.ps1"

    What it does:
      1. Copies the agent tree to C:\Users\<you>\.legion-control\agent
      2. Points a scheduled task at the agent's update cycle, every 15 minutes.
         If the task named by -TaskName does not exist it is created, running as
         the account this script runs under. If it does exist, its action and
         its repetition are rewritten and everything else about it — principal,
         time limit, conditions — is left exactly as found.
      3. Retires a superseded update-nightly.ps1 if one is still lying around,
         by renaming it. Nothing is ever deleted.

    What it deliberately does NOT touch:
      Any other task on the machine. A task that starts a service the agent
      manages, a watchdog that supervises it, or anything that arms the
      firmware at boot is none of this script's business. The agent starts and
      stops services through the config, and it never registers tasks.

    Elevation. A task that runs as SYSTEM cannot be re-registered without an
    elevated session; a task that runs as the current user can. Worth knowing,
    because it is the opposite of what you would expect: an SSH login as a
    member of the Administrators group IS elevated. Windows OpenSSH does not
    apply UAC filtering to those sessions, so there is no prompt to answer.

    The script still handles the unelevated case rather than relying on that,
    because it also has to work when run from a normal desktop PowerShell where
    UAC filtering does apply. So when it is rewriting an existing task it always
    writes the finished task XML to disk, then either registers it right away
    when the session is elevated, or prints the exact one line command to paste
    into an elevated PowerShell.

    Safe to re-run.
#>

[CmdletBinding()]
param(
    # The scheduled task that runs the update cycle. Created if it is missing.
    [string] $TaskName = 'Legion Control Update',

    # Skip the file copy. Useful when only the scheduled task needs fixing.
    [switch] $SkipCopy
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------

$TaskNamespace = 'http://schemas.microsoft.com/windows/2004/02/mit/task'
$Interval      = 'PT15M'
$IntervalSpan  = New-TimeSpan -Minutes 15
$MinNodeMajor  = 22

$UserProfile = $env:USERPROFILE
$Base        = Join-Path $UserProfile '.legion-control'
$AgentDir    = Join-Path $Base 'agent'
$AgentEntry  = Join-Path $AgentDir 'src\index.mjs'
$XmlPath     = Join-Path $Base (($TaskName -replace '[^\w\-]', '-') + '.xml')

# Where the superseded PowerShell updater used to live, if this machine ever ran
# one. Only read, and only renamed.
$LegacyDir   = Join-Path $UserProfile 'AppData\Local\T3Code'
$OldUpdater  = Join-Path $LegacyDir 'update-nightly.ps1'

$Summary = New-Object System.Collections.Generic.List[string]

function Say([string] $Text) {
    Write-Host $Text
}

function Note([string] $Text) {
    $Summary.Add($Text) | Out-Null
    Write-Host ("  " + $Text)
}

function Step([string] $Text) {
    Write-Host ""
    Write-Host ("== " + $Text)
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

Step 'Preflight'

$identity   = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal  = New-Object Security.Principal.WindowsPrincipal($identity)
$IsElevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
Note ("running as " + $identity.Name + ", elevated=" + $IsElevated)

# The controller apps invoke the agent over SSH without quoting, which is only
# safe while the install paths stay free of spaces. Fail loudly rather than
# produce a layout that breaks in a way nobody would connect back to this script.
if ($Base -match '\s') {
    throw "The install path '$Base' contains a space. The agent contract requires space free paths."
}

$nodeCmd = Get-Command 'node.exe' -ErrorAction SilentlyContinue
if (-not $nodeCmd) {
    $nodeCmd = Get-Command 'node' -ErrorAction SilentlyContinue
}
if (-not $nodeCmd) {
    throw "node is not on PATH for this session. Install Node.js or fix PATH before running this."
}
$NodeExe = $nodeCmd.Source
if (-not $NodeExe) {
    throw "Get-Command found node but reported no path for it."
}

$nodeVersion = (& $NodeExe -v) | Select-Object -First 1     # e.g. v24.18.0
$nodeMajor   = 0
if ($nodeVersion -match '^v(\d+)\.') {
    $nodeMajor = [int] $Matches[1]
} else {
    throw "Could not parse the node version from '$nodeVersion'."
}
if ($nodeMajor -lt $MinNodeMajor) {
    throw "node $nodeVersion is too old. The agent needs node >= $MinNodeMajor for node:sqlite."
}
Note ("node $nodeVersion at $NodeExe")

# A task can be registered to run as SYSTEM, whose PATH is the machine PATH and
# does not include a per user shim directory. Pin the absolute node path into
# the task instead of trusting that "node" resolves over there.
if ($NodeExe.ToLowerInvariant().StartsWith($UserProfile.ToLowerInvariant())) {
    Note ("WARNING: node lives inside the user profile. A task running as another")
    Note ("         principal can still run it, but a per user Node manager would")
    Note ("         break the task the moment it moves.")
}

# ---------------------------------------------------------------------------
# Agent tree
# ---------------------------------------------------------------------------

Step 'Agent'

$SrcAgent = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path (Join-Path $SrcAgent 'src\index.mjs'))) {
    throw "No agent source at $SrcAgent\src\index.mjs. Copy the repo across first."
}

$sameTree = $false
if (Test-Path $AgentDir) {
    $a = (Resolve-Path $SrcAgent).ProviderPath.TrimEnd('\')
    $b = (Resolve-Path $AgentDir).ProviderPath.TrimEnd('\')
    $sameTree = $a.Equals($b, [StringComparison]::OrdinalIgnoreCase)
}

if ($SkipCopy) {
    Note 'copy skipped by request'
} elseif ($sameTree) {
    # Running the installer out of the deployed copy. Wiping the destination
    # would delete this script while it is executing.
    Note "agent source is already $AgentDir, skipping the copy"
} else {
    if (-not (Test-Path $Base)) {
        New-Item -ItemType Directory -Path $Base -Force | Out-Null
    }
    if (Test-Path $AgentDir) {
        Remove-Item -Path $AgentDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $AgentDir -Force | Out-Null
    Copy-Item -Path (Join-Path $SrcAgent 'src') -Destination (Join-Path $AgentDir 'src') -Recurse -Force
    if (Test-Path (Join-Path $SrcAgent 'install')) {
        # Ship the installer along so the box can be re-provisioned from itself.
        Copy-Item -Path (Join-Path $SrcAgent 'install') -Destination (Join-Path $AgentDir 'install') -Recurse -Force
    }
    Note "agent copied to $AgentDir"
}

if (-not (Test-Path $AgentEntry)) {
    throw "Agent entry point missing at $AgentEntry."
}

# config.json and state.json are deliberately not written here. The agent falls
# back to its own defaults when they are absent, and stamping a fresh config
# over the top would silently flip auto update back on at every install.
if (Test-Path (Join-Path $Base 'config.json')) {
    Note 'config.json is already there and was left untouched'
} else {
    Note 'no config.json; the agent falls back to its defaults until you write one'
}

# Smoke test the agent. Only stdout is captured, deliberately: the agent puts
# one JSON object there and everything chatty on stderr, and folding stderr in
# with 2>&1 while ErrorActionPreference is Stop makes PowerShell 5.1 raise a
# NativeCommandError for perfectly normal log lines.
try {
    $agentVersionOutput = (& $NodeExe $AgentEntry version) -join ' '
    if ($LASTEXITCODE -ne 0) {
        Note ("WARNING: the agent exited with $LASTEXITCODE on 'version': " + $agentVersionOutput)
    } else {
        Note ("agent responds: " + $agentVersionOutput)
    }
} catch {
    Note ("WARNING: 'node $AgentEntry version' failed: " + $_.Exception.Message)
}

# ---------------------------------------------------------------------------
# Scheduled task
# ---------------------------------------------------------------------------

Step 'Scheduled task'

$existing       = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
$TaskRegistered = $false
$RegisterCommand = $null
$taskPath       = '\'

if (-not $existing) {

    # --- create it ----------------------------------------------------------

    # A task that runs as this account needs no elevation to register, which is
    # why nothing here reaches for SYSTEM. The agent only ever touches files and
    # services this account owns.
    Note "'$TaskName' does not exist, creating it"

    $action = New-ScheduledTaskAction -Execute $NodeExe -Argument "$AgentEntry update" -WorkingDirectory $Base

    # A one-off trigger in the past with a repetition and no duration repeats
    # forever, and StartWhenAvailable makes a run the machine slept through
    # happen once it is awake instead of waiting for the next boundary.
    $trigger = New-ScheduledTaskTrigger -Once -At (Get-Date).AddMinutes(1) -RepetitionInterval $IntervalSpan

    # S4U runs the task whether or not anyone is signed in and stores no
    # password. Interactive is the fallback for an account that is not allowed
    # to log on as a batch job.
    $taskPrincipal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType S4U -RunLevel Limited

    # A cycle that installs and waits for health can outlive a 15 minute window.
    # If the task were allowed to run in parallel, two installs would fight over
    # the same files and wreck the install. IgnoreNew is the only sane policy
    # here, and the time limit matches: a run that has not finished in 15
    # minutes is stuck, and the agent's lock is written with a pid so the next
    # run can take it over.
    $settings = New-ScheduledTaskSettingsSet `
        -MultipleInstances IgnoreNew `
        -ExecutionTimeLimit (New-TimeSpan -Minutes 15) `
        -AllowStartIfOnBatteries `
        -DontStopIfGoingOnBatteries `
        -StartWhenAvailable

    try {
        Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
            -Principal $taskPrincipal -Settings $settings -Force | Out-Null
    } catch {
        Note ("S4U registration failed (" + $_.Exception.Message + "), retrying as an interactive task")
        $taskPrincipal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Limited
        Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
            -Principal $taskPrincipal -Settings $settings -Force | Out-Null
    }

    $existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction Stop
    if ($existing.TaskPath) { $taskPath = $existing.TaskPath }
    $TaskRegistered = $true
    Note ("created '$TaskName': " + $NodeExe + " " + $AgentEntry + " update, every 15 minutes as " + $identity.Name)

} else {

    # --- rewrite what is there ---------------------------------------------

    if ($existing.TaskPath) { $taskPath = $existing.TaskPath }
    $FullTaskName = ($taskPath.TrimEnd('\') + '\' + $existing.TaskName).TrimStart('\')

    $xmlText = Export-ScheduledTask -TaskName $existing.TaskName -TaskPath $taskPath

    $doc = New-Object System.Xml.XmlDocument
    $doc.PreserveWhitespace = $true
    $doc.LoadXml($xmlText)

    $ns = New-Object System.Xml.XmlNamespaceManager($doc.NameTable)
    $ns.AddNamespace('t', $TaskNamespace) | Out-Null

    # --- report what is there now, before anything is changed ---------------

    $oldCommand   = $doc.SelectSingleNode('/t:Task/t:Actions/t:Exec/t:Command', $ns)
    $oldArguments = $doc.SelectSingleNode('/t:Task/t:Actions/t:Exec/t:Arguments', $ns)
    $oldIntervals = $doc.SelectNodes('/t:Task/t:Triggers/*/t:Repetition/t:Interval', $ns)

    $oldActionText = 'none'
    if ($oldCommand) {
        $oldActionText = $oldCommand.InnerText
        if ($oldArguments) { $oldActionText = $oldActionText + ' ' + $oldArguments.InnerText }
    }
    Note ("old action:   " + $oldActionText)

    $oldIntervalText = 'none'
    if ($oldIntervals.Count -gt 0) {
        $oldIntervalText = (($oldIntervals | ForEach-Object { $_.InnerText }) -join ', ')
    }
    Note ("old interval: " + $oldIntervalText)

    $principalNode = $doc.SelectSingleNode('/t:Task/t:Principals/t:Principal/t:UserId', $ns)
    $runsAsSystem  = $false
    if ($principalNode) {
        # Left exactly as found. S-1-5-18 is SYSTEM.
        Note ("principal:    " + $principalNode.InnerText + " (preserved)")
        $runsAsSystem = ($principalNode.InnerText -eq 'S-1-5-18')
    }

    $limitNode = $doc.SelectSingleNode('/t:Task/t:Settings/t:ExecutionTimeLimit', $ns)
    if ($limitNode) {
        Note ("time limit:   " + $limitNode.InnerText + " (preserved)")
    }

    # --- action -------------------------------------------------------------

    $actions = $doc.SelectSingleNode('/t:Task/t:Actions', $ns)
    if (-not $actions) {
        throw "The exported XML for '$TaskName' has no Actions element. Refusing to guess."
    }

    while ($actions.HasChildNodes) {
        $actions.RemoveChild($actions.FirstChild) | Out-Null
    }

    $execNode = $doc.CreateElement('Exec', $TaskNamespace)

    $commandNode = $doc.CreateElement('Command', $TaskNamespace)
    $commandNode.InnerText = $NodeExe
    $execNode.AppendChild($commandNode) | Out-Null

    # The path carries no spaces (checked in preflight), so it needs no quoting.
    # Keeping it unquoted matches how the controller invokes the agent over SSH.
    $argumentsNode = $doc.CreateElement('Arguments', $TaskNamespace)
    $argumentsNode.InnerText = "$AgentEntry update"
    $execNode.AppendChild($argumentsNode) | Out-Null

    # A task running as SYSTEM starts in C:\Windows\system32. Point it somewhere
    # the agent owns so anything written relative to the working directory lands
    # sensibly.
    $workingNode = $doc.CreateElement('WorkingDirectory', $TaskNamespace)
    $workingNode.InnerText = $Base
    $execNode.AppendChild($workingNode) | Out-Null

    $actions.AppendChild($execNode) | Out-Null
    Note ("new action:   " + $NodeExe + " " + $AgentEntry + " update")

    # --- triggers -----------------------------------------------------------

    # Why 15 minutes: the update cycle refuses to touch the machine while it is
    # busy and reports "deferred", so the retry cadence is also the recovery
    # time. At a quarter hour a machine that has just gone idle picks up the
    # waiting release almost straight away. At an hour it could sit on a stale
    # build for most of an evening for no reason at all.
    $triggers = $doc.SelectSingleNode('/t:Task/t:Triggers', $ns)
    if (-not $triggers) {
        throw "The exported XML for '$TaskName' has no Triggers element. Delete the task and rerun this script to have it created from scratch."
    }

    $triggerNodes = $triggers.SelectNodes('*')
    if ($triggerNodes.Count -eq 0) {
        throw "'$TaskName' has no triggers at all. Delete the task and rerun this script to have it created from scratch."
    }

    # Only retune triggers that already repeat. Bolting a repetition onto, say, a
    # boot trigger that never had one would quietly double the schedule instead of
    # changing it, which is not what this script is for.
    $repeating = @()
    foreach ($trigger in $triggerNodes) {
        if ($trigger.SelectSingleNode('t:Repetition', $ns)) {
            $repeating += $trigger
        }
    }
    if ($repeating.Count -eq 0) {
        # Nothing repeats today, so the first trigger becomes the repeating one.
        $repeating = @($triggerNodes[0])
        Note ("no trigger repeated before, adding a repetition to the " + $triggerNodes[0].LocalName)
    }

    foreach ($trigger in $repeating) {
        # Task Scheduler validates element order on import and rejects anything out
        # of sequence, so new nodes go in at the front: Repetition is the first
        # child of a trigger, and Interval is the first child of Repetition.
        $rep = $trigger.SelectSingleNode('t:Repetition', $ns)
        if (-not $rep) {
            $rep = $doc.CreateElement('Repetition', $TaskNamespace)
            $trigger.PrependChild($rep) | Out-Null
        }

        $intervalNode = $rep.SelectSingleNode('t:Interval', $ns)
        if (-not $intervalNode) {
            $intervalNode = $doc.CreateElement('Interval', $TaskNamespace)
            $rep.PrependChild($intervalNode) | Out-Null
        }
        $intervalNode.InnerText = $Interval

        # A Duration left over from an hourly schedule would cap the repetition:
        # with PT15M inside a PT1H duration the task fires four times and then
        # goes quiet until the next start boundary. Removing it means repeat
        # forever, and StopAtDurationEnd has to go false to match, since stopping
        # at the end of a duration that no longer exists is a contradiction the
        # importer may reject.
        $durationNode = $rep.SelectSingleNode('t:Duration', $ns)
        if ($durationNode) {
            $droppedDuration = $durationNode.InnerText
            $rep.RemoveChild($durationNode) | Out-Null
            Note ("dropped a repetition Duration of " + $droppedDuration + " so the 15 minute repeat runs indefinitely")
        }
        $stopNode = $rep.SelectSingleNode('t:StopAtDurationEnd', $ns)
        if ($stopNode) {
            $stopNode.InnerText = 'false'
        }
    }
    Note ("new interval: " + $Interval + " on " + $repeating.Count + " of " + $triggerNodes.Count + " trigger(s)")

    # --- overlap policy -----------------------------------------------------

    # A cycle that installs and waits for health can outlive a 15 minute window.
    # If the task were allowed to run in parallel, two installs would fight over
    # the same files and wreck the install. IgnoreNew is the only sane policy.
    $policyNode = $doc.SelectSingleNode('/t:Task/t:Settings/t:MultipleInstancesPolicy', $ns)
    if ($policyNode) {
        if ($policyNode.InnerText -ne 'IgnoreNew') {
            Note ("overlap policy was " + $policyNode.InnerText + ", set to IgnoreNew")
            $policyNode.InnerText = 'IgnoreNew'
        } else {
            Note 'overlap policy already IgnoreNew'
        }
    } else {
        # Not inserting one by hand: Settings has a strict element order and a
        # misplaced node fails the whole import.
        Note 'WARNING: no MultipleInstancesPolicy in the task XML, check it by hand in taskschd.msc'
    }

    # --- write the XML ------------------------------------------------------

    if (-not (Test-Path $Base)) {
        New-Item -ItemType Directory -Path $Base -Force | Out-Null
    }

    # Save() honours the encoding in the XML declaration, which Export-ScheduledTask
    # emits as UTF-16. Writing it any other way gives schtasks a file whose bytes
    # disagree with its own header.
    $doc.Save($XmlPath)
    Note ("task XML written to " + $XmlPath)

    # --- register -----------------------------------------------------------

    $RegisterCommand = 'schtasks /Create /TN "' + $FullTaskName + '" /XML "' + $XmlPath + '" /F'

    if ($IsElevated -or -not $runsAsSystem) {
        # schtasks rather than Register-ScheduledTask, for one reason: the command
        # run here is then character for character the command printed further down
        # for the unelevated case, so there is only ever one thing to debug.
        #
        # PowerShell 5.1 turns any stderr line from a native command into a
        # terminating NativeCommandError while ErrorActionPreference is Stop, and
        # schtasks reports its failures on stderr. Relax it just around the call so
        # the real exit code and message can be read instead of a wrapper exception.
        $exitCode = -1
        $output = @()
        $previousPreference = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $output = & schtasks.exe /Create /TN $FullTaskName /XML $XmlPath /F 2>&1
            $exitCode = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $previousPreference
        }

        if ($exitCode -ne 0) {
            Say ''
            Say (($output | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine)
            throw "schtasks exited with $exitCode. The XML it rejected is at $XmlPath. Check the task in taskschd.msc before rerunning, then: $RegisterCommand"
        }
        $TaskRegistered = $true
        Note "task '$FullTaskName' re-registered"
    } else {
        Note "NOT registered: this task runs as SYSTEM and this session is not elevated."
    }
}

# ---------------------------------------------------------------------------
# Superseded updater script
# ---------------------------------------------------------------------------

Step 'Old updater'

# Order matters here. Until the task has actually been re-registered it may
# still be pointing at the old script, so renaming that file first would leave a
# live task calling something that is not there any more. Only retire it once
# the task really points somewhere else.
if (-not (Test-Path $OldUpdater)) {
    Note 'no superseded updater script to retire'
} elseif (-not $TaskRegistered) {
    Note 'update-nightly.ps1 left in place: the task still points at it until the elevated command below has run'
} else {
    $superseded = $OldUpdater + '.superseded'
    if (Test-Path $superseded) {
        # Never overwrite an earlier backup, keep both.
        $superseded = $OldUpdater + '.superseded.' + (Get-Date -Format 'yyyyMMdd-HHmmss')
    }
    Move-Item -Path $OldUpdater -Destination $superseded
    Note ("renamed update-nightly.ps1 to " + (Split-Path -Leaf $superseded) + ", nothing was deleted")
    Note ("it still sits in " + $LegacyDir + " if the old behaviour is ever needed back")
}

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------

Step 'Verify'

if ($TaskRegistered) {
    $checkXml = New-Object System.Xml.XmlDocument
    $checkXml.LoadXml((Export-ScheduledTask -TaskName $existing.TaskName -TaskPath $taskPath))
    $checkNs = New-Object System.Xml.XmlNamespaceManager($checkXml.NameTable)
    $checkNs.AddNamespace('t', $TaskNamespace) | Out-Null

    $liveCommand  = $checkXml.SelectSingleNode('/t:Task/t:Actions/t:Exec/t:Command', $checkNs)
    $liveArgs     = $checkXml.SelectSingleNode('/t:Task/t:Actions/t:Exec/t:Arguments', $checkNs)
    $liveInterval = $checkXml.SelectSingleNode('/t:Task/t:Triggers/*/t:Repetition/t:Interval', $checkNs)
    $liveUser     = $checkXml.SelectSingleNode('/t:Task/t:Principals/t:Principal/t:UserId', $checkNs)
    $liveLimit    = $checkXml.SelectSingleNode('/t:Task/t:Settings/t:ExecutionTimeLimit', $checkNs)

    if ($liveCommand)  { Note ("live action:    " + $liveCommand.InnerText + ' ' + $(if ($liveArgs) { $liveArgs.InnerText } else { '' })) }
    if ($liveInterval) { Note ("live interval:  " + $liveInterval.InnerText) }
    if ($liveUser)     { Note ("live principal: " + $liveUser.InnerText) }
    if ($liveLimit)    { Note ("live limit:     " + $liveLimit.InnerText) }
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

Say ''
Say '== Legion Control, Windows side'
Say ''
foreach ($line in $Summary) {
    Say ('  ' + $line)
}

if (-not $TaskRegistered -and $RegisterCommand) {
    Say ''
    Say '  ONE STEP LEFT. The task XML is ready but this task runs as SYSTEM and'
    Say '  can only be registered from an elevated session, which this is not. An'
    Say '  ssh login as an administrator normally is elevated, so if you are seeing'
    Say '  this from ssh something is unusual. Open PowerShell as administrator on'
    Say '  the machine itself and run exactly this:'
    Say ''
    Say ('      ' + $RegisterCommand)
    Say ''
    Say '  Then rerun this script to see the verification output.'
}

Say ''
Say ('  Agent:  ' + $NodeExe + ' ' + $AgentEntry + ' status')
Say ('  Config: ' + (Join-Path $Base 'config.json'))
Say ('  Task:   Get-ScheduledTaskInfo -TaskName "' + $TaskName + '"')
Say ('  Logs:   ' + (Join-Path $Base 'legionctl.log'))
Say ''

# Exit codes, so a script driving this over SSH can tell the outcomes apart
# instead of reading the prose:
#   0  fully installed, the task now runs the agent
#   2  agent installed, but the task still needs the elevated command above
#   1  something threw, see the message
if (-not $TaskRegistered) {
    exit 2
}
exit 0

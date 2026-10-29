#Requires -Version 5.1
<#
    Legion Control: install the agent on a Windows system and hand the update
    job over to it.

    Run it over SSH as the account that owns the install:

        ssh <machine> "powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\<you>\legion-control\agent\install\install-windows.ps1"

    What it does:
      1. Gives the base (LEGIONCTL_HOME, normally .legion-control in your
         profile) to your account alone, then stages and self-checks the
         complete agent tree before replacing the live one.
      2. Points a scheduled task at the agent's update cycle, every 15 minutes,
         running as you. If the task named by -TaskName does not exist it is
         created. If it does exist, its action, its repetition and its time
         limit are rewritten and everything else about it (principal,
         conditions) is left exactly as found.
      3. Says which shell OpenSSH runs commands with, which decides what the
         setup has to say for this system's "shell".

    -Uninstall takes the task, the launchers and the agent trees away again,
    and leaves what the agent was configured with and what it recorded.
    -Uninstall -Purge removes those as well.

    What it deliberately does NOT touch:
      Any other task on the machine. A task that starts a service the agent
      manages, a watchdog that supervises it, or anything that arms the
      firmware at boot is none of this script's business. The agent starts and
      stops services through the config, and it never registers tasks.

    Principal. The task runs the agent as the account that installs it, the
    account whose base holds the launcher, the agent tree and config.json.
    Anything that can write those runs code as the task's principal, so a task
    running as anybody else would hand that account's rights to whoever can
    write them, and a task running as SYSTEM out of a profile hands them the
    machine. An existing task with any other principal is refused before
    anything is changed.

    -AllowSystemPrincipal keeps an existing task that runs as SYSTEM, on these
    terms only: the session is elevated, the base and node are outside every
    user profile, nobody but Administrators and SYSTEM may rename or
    re-permission a folder above the base, and the base is then given to
    Administrators and SYSTEM alone. Nothing a user can write is run as SYSTEM.

    Elevation. A task that runs as the current user can be registered and
    re-registered without it, so only -AllowSystemPrincipal needs an elevated
    session. Worth knowing, because it is the opposite of what you would
    expect: an SSH login as a member of the Administrators group IS elevated.
    Windows OpenSSH does not apply UAC filtering to those sessions, so there is
    no prompt to answer.

    Safe to re-run.
#>

# Write-Host on purpose: what this prints is for the person running it, over ssh
# or at a prompt, and none of it is output for a pipeline. The file stays ASCII,
# because Windows PowerShell reads a script without a byte order mark in the
# ANSI code page.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '')]
[CmdletBinding()]
param(
    # The scheduled task that runs the update cycle. Created if it is missing.
    [string] $TaskName = 'Legion Control Update',

    # Skip the file copy. Useful when only the scheduled task needs fixing.
    [switch] $SkipCopy,

    # Install and verify files without creating or changing a scheduled task.
    [switch] $SkipScheduler,

    # Keep an existing task that runs as SYSTEM, on the terms described above.
    [switch] $AllowSystemPrincipal,

    # Remove the task, the launchers and the agent trees instead of installing.
    [switch] $Uninstall,

    # With -Uninstall, remove what the agent was configured with and what it
    # recorded as well.
    [switch] $Purge
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Settings
# ---------------------------------------------------------------------------

$TaskNamespace = 'http://schemas.microsoft.com/windows/2004/02/mit/task'
$Interval      = 'PT15M'
$IntervalSpan  = New-TimeSpan -Minutes 15
# How long one cycle may run before the task stops it: the cycle's budget, the
# same 30 minutes as TimeoutStartSec=1800 in the systemd unit.
$TimeLimit     = 'PT30M'
$TimeLimitSpan = New-TimeSpan -Minutes 30
$MinNodeMajor  = 24

# Well-known SIDs, which mean the same in every Windows language.
$SystemSid           = 'S-1-5-18'
$AdministratorsSid   = 'S-1-5-32-544'
$TrustedInstallerSid = 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'

$UserProfile = $env:USERPROFILE
$Base        = if ($env:LEGIONCTL_HOME) { $env:LEGIONCTL_HOME } else { Join-Path $UserProfile '.legion-control' }
if (-not [IO.Path]::IsPathRooted($Base)) {
    throw "LEGIONCTL_HOME must be an absolute path, got '$Base'."
}
$Base        = [IO.Path]::GetFullPath($Base).TrimEnd([IO.Path]::DirectorySeparatorChar)
$env:LEGIONCTL_HOME = $Base
$AgentDir    = Join-Path $Base 'agent'
$AgentEntry  = Join-Path $AgentDir 'src\index.mjs'
$BinDir      = Join-Path $Base 'bin'
$LauncherModule = Join-Path $BinDir 'launcher.mjs'
$LauncherScript = Join-Path $BinDir 'legionctl.ps1'

# Where an older installer left the task XML for an elevated session to import
# by hand. Nothing writes it any more, and one still lying around is removed:
# whoever could write it chose what that import would have registered.
$OldXmlPath  = Join-Path $Base (($TaskName -replace '[^\w\-]', '-') + '.xml')

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

# PowerShell 5.1 turns any stderr line from a native command into a terminating
# NativeCommandError while ErrorActionPreference is Stop, and schtasks and
# icacls report their failures on stderr. Relax it just around the call so the
# real exit code and message can be read instead of a wrapper exception.
function Invoke-Native([string] $Command, [string[]] $Arguments) {
    $exitCode = -1
    $output = @()
    $previousPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $output = & $Command @Arguments 2>&1
        $exitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousPreference
    }
    return [pscustomobject] @{
        ExitCode = $exitCode
        Output   = (@($output) | ForEach-Object { $_.ToString() }) -join [Environment]::NewLine
    }
}

# The SID an account name or SID stands for, or $null for an account this
# machine cannot name. Task Scheduler hands back either form.
function Resolve-AccountSid([string] $Account) {
    if ($Account -match '^S-1-[0-9]+(-[0-9]+)+$') {
        return $Account
    }
    try {
        return (New-Object Security.Principal.NTAccount($Account)).Translate([Security.Principal.SecurityIdentifier]).Value
    } catch {
        return $null
    }
}

# Every folder a user profile lives in: the folder profiles are made in, and
# every profile the machine knows, wherever it is. The service accounts' own
# profiles are not user profiles and are left out.
function Get-ProfileRoot {
    $list = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList'
    $roots = New-Object System.Collections.Generic.List[string]
    $roots.Add($UserProfile)
    $directory = (Get-ItemProperty -LiteralPath $list -ErrorAction SilentlyContinue).ProfilesDirectory
    if ($directory) { $roots.Add([Environment]::ExpandEnvironmentVariables($directory)) }
    foreach ($key in @(Get-ChildItem -LiteralPath $list -ErrorAction SilentlyContinue)) {
        if (@($SystemSid, 'S-1-5-19', 'S-1-5-20') -contains $key.PSChildName) { continue }
        $image = (Get-ItemProperty -LiteralPath $key.PSPath -ErrorAction SilentlyContinue).ProfileImagePath
        if ($image) { $roots.Add([Environment]::ExpandEnvironmentVariables($image)) }
    }
    return $roots
}

# Whether $Path is one of $Roots or lies inside one.
function Test-Inside([string] $Path, [string[]] $Roots) {
    $full = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    foreach ($root in $Roots) {
        if (-not $root) { continue }
        $prefix = [IO.Path]::GetFullPath($root).TrimEnd('\')
        if ($full.Equals($prefix, [StringComparison]::OrdinalIgnoreCase) -or
            $full.StartsWith($prefix + '\', [StringComparison]::OrdinalIgnoreCase)) {
            return $true
        }
    }
    return $false
}

# Who besides Administrators and SYSTEM could put another folder where $Path
# is: whoever owns a folder above it or may delete, rename or re-permission one,
# or what it holds, and any link on the way. Nobody, when the list is empty.
function Get-PathTakeover([string] $Path) {
    $trusted = @($SystemSid, $AdministratorsSid, $TrustedInstallerSid)
    # Generic all and generic write, besides the rights that move or re-permission a folder.
    $dangerous = [int] ([Security.AccessControl.FileSystemRights] 'Delete, DeleteSubdirectoriesAndFiles, ChangePermissions, TakeOwnership') -bor 0x10000000 -bor 0x40000000
    $found = New-Object System.Collections.Generic.List[string]
    if ((Test-Path -LiteralPath $Path) -and (Get-Item -LiteralPath $Path -Force).Attributes.HasFlag([IO.FileAttributes]::ReparsePoint)) {
        $found.Add("$Path is a link")
    }
    $folder = Split-Path -Parent $Path
    while ($folder) {
        # A folder that is not there yet is made by this elevated session, as
        # Administrators', inside the first one that is.
        if (Test-Path -LiteralPath $folder) {
            if ((Get-Item -LiteralPath $folder -Force).Attributes.HasFlag([IO.FileAttributes]::ReparsePoint)) {
                $found.Add("$folder is a link")
            }
            $acl = Get-Acl -LiteralPath $folder
            $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
            if ($trusted -notcontains $owner) {
                $found.Add("$owner owns $folder")
            }
            foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
                if ($rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow) { continue }
                if ($rule.PropagationFlags.HasFlag([Security.AccessControl.PropagationFlags]::InheritOnly)) { continue }
                if ($trusted -contains $rule.IdentityReference.Value) { continue }
                if (([int] $rule.FileSystemRights -band $dangerous) -ne 0) {
                    $found.Add("$($rule.IdentityReference.Value) may move or re-permission $folder")
                }
            }
        }
        $parent = Split-Path -Parent $folder
        if ($parent -eq $folder) { break }
        $folder = $parent
    }
    return $found
}

function Invoke-IcaclsCommand([string[]] $Arguments) {
    $result = Invoke-Native -Command 'icacls.exe' -Arguments $Arguments
    if ($result.ExitCode -ne 0) {
        throw ("icacls " + ($Arguments -join ' ') + " exited with " + $result.ExitCode + ": " + $result.Output)
    }
}

# Gives the base to $Sids alone: full control for each of them is all that is
# left on it, nothing comes from the folder above, and everything inside goes
# back to inheriting just that. With $Owner, that account owns every file and
# folder in it as well, so one made there by somebody else is no longer theirs
# to re-permission. The base is closed first and opened to nobody on the way.
# icacls is given SIDs, never names, which are spelled differently in another
# language.
function Protect-Base([string[]] $Sids, [string] $Owner) {
    if (-not (Test-Path -LiteralPath $Base -PathType Container)) {
        New-Item -ItemType Directory -Path $Base -Force | Out-Null
    }
    $grants = @($Sids | ForEach-Object { '*' + $_ + ':(OI)(CI)F' })
    Invoke-IcaclsCommand -Arguments (@($Base, '/inheritance:r', '/grant:r') + $grants)

    # /grant:r replaces what the named accounts had, and leaves an entry
    # somebody added for another account where it is.
    $acl = Get-Acl -LiteralPath $Base
    foreach ($rule in $acl.GetAccessRules($true, $false, [Security.Principal.SecurityIdentifier])) {
        if ($Sids -notcontains $rule.IdentityReference.Value) {
            Invoke-IcaclsCommand -Arguments @($Base, '/remove', ('*' + $rule.IdentityReference.Value))
        }
    }

    if ($Owner) {
        Invoke-IcaclsCommand -Arguments @($Base, '/setowner', ('*' + $Owner), '/T', '/C', '/Q')
    }
    if (Get-ChildItem -LiteralPath $Base -Force | Select-Object -First 1) {
        Invoke-IcaclsCommand -Arguments @((Join-Path $Base '*'), '/reset', '/T', '/C', '/Q')
    }

    $acl = Get-Acl -LiteralPath $Base
    $holders = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]) | ForEach-Object { $_.IdentityReference.Value } | Sort-Object -Unique)
    $wanted = @($Sids | Sort-Object -Unique)
    if (-not $acl.AreAccessRulesProtected -or ($holders -join ',') -ne ($wanted -join ',')) {
        throw ("$Base could not be given to " + ($wanted -join ', ') + " alone; it lists " + ($holders -join ', ') + ". Nothing was installed.")
    }
    if ($Owner -and $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $Owner) {
        throw "$Base could not be given to the owner $Owner. Nothing was installed."
    }
}

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------

Step 'Preflight'

$identity   = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal  = New-Object Security.Principal.WindowsPrincipal($identity)
$IsElevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
$UserSid    = $identity.User.Value
Note ("running as " + $identity.Name + " (" + $UserSid + "), elevated=" + $IsElevated)

if ($Base.Contains("`n") -or $Base.Contains("`r")) {
    throw 'Install paths containing a line break are unsupported.'
}
if ($Uninstall -and ($SkipCopy -or $SkipScheduler -or $AllowSystemPrincipal)) {
    throw '-Uninstall takes -TaskName and -Purge, and nothing else.'
}
if ($Purge -and -not $Uninstall) {
    throw '-Purge only goes with -Uninstall.'
}

# The base is a folder of the agent's own. Installing gives it and everything
# in it to one account alone, and uninstalling takes out what it knows by name,
# so a drive, a profile, a folder profiles are kept in or one Windows and
# programs keep their own files in would lose every other account's access, or
# its bin folder. Refused before either.
$sharedFolders = @(Get-ProfileRoot) + @($env:SystemRoot, $env:ProgramFiles, ${env:ProgramFiles(x86)}, $env:ProgramData, $env:APPDATA, $env:LOCALAPPDATA)
if ($Base -eq [IO.Path]::GetPathRoot($Base + '\').TrimEnd('\') -or
    @($sharedFolders | Where-Object { $_ -and (Test-Inside $_ @($Base)) }).Count -gt 0) {
    throw ("LEGIONCTL_HOME is $Base, which is not a folder of the agent's own. Point it at one, such as " +
        (Join-Path $UserProfile '.legion-control') + ", and run this again. Nothing was changed.")
}

$nodeCmd =Get-Command 'node.exe' -ErrorAction SilentlyContinue
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

# The task does not start with this session's PATH, which may hold a per user
# shim directory. Pin the absolute node path into the task instead of trusting
# that "node" resolves over there.
if ($NodeExe.ToLowerInvariant().StartsWith($UserProfile.ToLowerInvariant())) {
    Note ("WARNING: node lives inside the user profile. A per user Node manager")
    Note ("         would break the task the moment it moves node.")
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

# Takes the agent's own entries out of the base, under the operation lock
# every mutating agent command takes: the same code as remove_agent_files in
# install-linux.sh, which says what it removes and why.
$RemoveAgentFiles = @'
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';

const [base, scope] = process.argv.slice(2);
const CODE = /^(?:bin|agent|agent\..+|agent-swap\.json|agent-baseline\.json)$/;
const DATA = /^(?:config\..+|controller\..+|agent-floor\.json.*|state|state\.json.*|ops|incoming|legionctl\.log.*|launchd\.log|update\.lock.*|op\.lock.*)$/;
const say = (line) => process.stdout.write(`${line}\n`);

// Moves all of `names` into one folder in the base, or none of them, and
// returns that folder. On Windows a file in use, or a folder a process works
// in, cannot be moved, and deleting one entry after another would stop there
// with the job still running what is left. The folder is named like an agent
// tree, so the next uninstall takes it if it outlives this one.
function moveAside(names) {
  const aside = path.join(base, `agent.removed.${process.pid}.${Date.now()}`);
  fs.mkdirSync(aside);
  const moved = [];
  try {
    for (const name of names) {
      fs.renameSync(path.join(base, name), path.join(aside, name));
      moved.push(name);
    }
  } catch (error) {
    const stuck = moved.reverse().filter((name) => {
      try { fs.renameSync(path.join(aside, name), path.join(base, name)); return false; }
      catch { return true; }
    });
    if (stuck.length > 0) throw new Error(`${error.message}; ${stuck.join(', ')} could not be put back from ${aside}`);
    fs.rmdirSync(aside);
    throw new Error(`${error.message}; nothing was removed`);
  }
  return aside;
}

function remove() {
  if (!fs.existsSync(base)) return;
  const db = new DatabaseSync(path.join(base, 'op.lock.sqlite'));
  try {
    db.exec('PRAGMA busy_timeout = 250');
    try { db.exec('BEGIN IMMEDIATE'); }
    catch (error) {
      if (error?.errcode === 5) throw new Error('another operation is running; nothing was removed');
      throw error;
    }
    const doomed = fs.readdirSync(base).filter((name) => (CODE.test(name) || (scope === 'purge' && DATA.test(name))) && !name.startsWith('op.lock')).sort();
    if (doomed.length > 0) {
      const aside = moveAside(doomed);
      for (const name of doomed) say(`removed ${path.join(base, name)}`);
      try { fs.rmSync(aside, { recursive: true, force: true }); }
      catch (error) { say(`could not delete all of ${aside} yet, and the next uninstall does: ${error.message}`); }
    }
  } finally {
    try { db.exec('ROLLBACK'); } catch { /* no transaction if acquisition failed */ }
    db.close();
  }
  if (scope !== 'purge') return;
  for (const name of fs.readdirSync(base).filter((each) => each.startsWith('op.lock'))) {
    try { fs.rmSync(path.join(base, name), { recursive: true, force: true }); }
    catch (error) { say(`could not remove ${path.join(base, name)}: ${error.message}`); }
  }
  const left = fs.readdirSync(base).sort();
  if (left.length === 0) {
    fs.rmdirSync(base);
    say(`removed ${base}`);
  }
  for (const name of left.filter((each) => !CODE.test(each) && !DATA.test(each))) say(`kept ${path.join(base, name)}, which is not the agent's`);
}

try { remove(); }
catch (error) {
  process.stderr.write(`${error.message}\n`);
  process.exitCode = 1;
}
'@

if ($Uninstall) {
    Step 'Uninstall'

    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if ($task) {
        $runs = (@($task.Actions) | ForEach-Object { [string] $_.Execute + ' ' + [string] $_.Arguments }) -join '; '
        if ($runs.IndexOf($Base, [StringComparison]::OrdinalIgnoreCase) -lt 0) {
            throw "'$TaskName' does not run the agent in $Base (it runs $runs), so it is not this script's to remove. Nothing was changed."
        }
        # A cycle that is running is left to finish. Stopped part way it can
        # leave a service down, and with the task gone nothing comes back to
        # recover it.
        if ($task.State -eq 'Running') {
            throw "'$TaskName' is running an update cycle right now. It finishes within 30 minutes; run this again then. Nothing was changed."
        }
        if ((Resolve-AccountSid ([string] $task.Principal.UserId)) -ne $UserSid -and -not $IsElevated) {
            throw "'$TaskName' runs as $($task.Principal.UserId)$($task.Principal.GroupId), and only an elevated session can remove it. Run this again from one. Nothing was changed."
        }
    }

    if (Test-Path -LiteralPath $OldXmlPath -PathType Leaf) {
        Remove-Item -LiteralPath $OldXmlPath -Force
        Note ("removed " + $OldXmlPath + ", which an older installer left for an elevated import")
    }

    # The files first, so an operation holding the lock stops the uninstall
    # before anything of the agent's has changed. A run the task starts in the
    # moment before it goes finds no launcher and fails, which does no harm.
    $scope = if ($Purge) { 'purge' } else { 'code' }
    $removed = @($RemoveAgentFiles | & $NodeExe --input-type=module - $Base $scope)
    if ($LASTEXITCODE -ne 0) {
        throw 'Uninstalling stopped there. The scheduled task was left as it was.'
    }
    foreach ($line in $removed) {
        if ($line) { Note $line }
    }

    if ($task) {
        Unregister-ScheduledTask -TaskName $task.TaskName -TaskPath $task.TaskPath -Confirm:$false
        Note "removed the scheduled task '$TaskName'"
    } else {
        Note "no scheduled task '$TaskName' to remove"
    }

    # This copy of the installer may have been the installed agent's and gone
    # with it, and then it is run again from a copy of the repository.
    $again = 'agent\install\install-windows.ps1'
    if (Test-Path -LiteralPath $PSCommandPath -PathType Leaf) { $again = $PSCommandPath }
    $again = 'powershell -NoProfile -ExecutionPolicy Bypass -File "' + $again + '"'
    if ($PSBoundParameters.ContainsKey('TaskName')) { $again = $again + ' -TaskName "' + $TaskName + '"' }

    Say ''
    Say '== Legion Control, Windows side removed'
    Say ''
    foreach ($line in $Summary) {
        Say ('  ' + $line)
    }
    if (-not $Purge -and (Test-Path -LiteralPath $Base)) {
        Say ''
        Say '  What the agent was configured with and what it recorded were left in:'
        Say ('    ' + $Base)
        Say ''
        Say '  Remove those too:'
        Say ('    ' + $again + ' -Uninstall -Purge')
    }
    foreach ($keys in @((Join-Path $UserProfile '.ssh\authorized_keys'), (Join-Path $env:ProgramData 'ssh\administrators_authorized_keys'))) {
        if ((Test-Path -LiteralPath $keys -PathType Leaf) -and (Select-String -LiteralPath $keys -SimpleMatch 'dispatch.ps1' -Quiet -ErrorAction SilentlyContinue)) {
            Say ''
            Say ('  A key in ' + $keys + ' runs dispatch.ps1, which is gone, so it is refused')
            Say '  from now on. Take its line out.'
        }
    }
    Say ''
    Say '  Put the agent back:'
    Say ('    ' + $again)
    Say ''
    exit 0
}

# ---------------------------------------------------------------------------
# Who the task runs as
# ---------------------------------------------------------------------------

# Judged before anything is changed, and with -SkipScheduler as well: what this
# script installs is what the task runs, whoever it runs as.
Step 'Principal'

$TaskRunsAsSystem = $false
$current = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
if (-not $current) {
    Note "'$TaskName' does not exist yet"
} else {
    $account = [string] $current.Principal.UserId
    $group   = [string] $current.Principal.GroupId
    $who     = if ($group) { "the group $group" } elseif ($account) { $account } else { 'an account this script cannot name' }
    $sid     = if ($group -or -not $account) { $null } else { Resolve-AccountSid $account }
    $removal = "Unregister-ScheduledTask -TaskName '" + $TaskName.Replace("'", "''") + "' -Confirm:`$false"

    if ($sid -eq $UserSid) {
        Note "'$TaskName' runs as $who, the account installing it"
    } elseif ($sid -eq $SystemSid -and $AllowSystemPrincipal) {
        $TaskRunsAsSystem = $true
        Note "'$TaskName' runs as SYSTEM, kept by -AllowSystemPrincipal"
    } elseif ($sid -eq $SystemSid) {
        throw ("'$TaskName' runs as SYSTEM, and whoever can write the agent's files would run code as SYSTEM through it. " +
            "Nothing was changed. Remove the task from an elevated session, and this script creates it again running as you: $removal. " +
            "Or keep it with -AllowSystemPrincipal, from an elevated session and with LEGIONCTL_HOME outside every user profile.")
    } else {
        throw ("'$TaskName' runs as $who, not as $($identity.Name), who is installing it, and whoever can write the agent's files would run code as $who through it. " +
            "Nothing was changed. Remove the task, and this script creates it again running as you: $removal")
    }
}

if ($TaskRunsAsSystem) {
    if (-not $IsElevated) {
        throw '-AllowSystemPrincipal needs an elevated session, which this is not. Nothing was changed.'
    }
    $profileRoots = @(Get-ProfileRoot)
    if (Test-Inside $Base $profileRoots) {
        throw ("The base $Base is inside a user profile, and that user could change what SYSTEM runs. " +
            "Set LEGIONCTL_HOME to a folder outside every profile, such as " + (Join-Path $env:ProgramData 'legion-control') + ", and run this again. Nothing was changed.")
    }
    if (Test-Inside $NodeExe $profileRoots) {
        throw "node at $NodeExe is inside a user profile, and that user could change what SYSTEM runs. Install Node.js for all users and run this again. Nothing was changed."
    }
    $takeover = @(Get-PathTakeover $Base)
    if ($takeover.Count -gt 0) {
        throw ("Somebody besides Administrators and SYSTEM could put another folder where the base is: " + ($takeover -join '; ') + ". Nothing was changed.")
    }
} elseif ($AllowSystemPrincipal) {
    Note '-AllowSystemPrincipal only keeps a task that already runs as SYSTEM; this one runs as you'
}

# ---------------------------------------------------------------------------
# Base
# ---------------------------------------------------------------------------

# Before anything goes in, so nothing in it is anyone else's from the start.
# The base holds every command the agent was configured with, the setup and the
# record of every operation, and the task runs what is in it.
Step 'Base'

if ($TaskRunsAsSystem) {
    Protect-Base -Sids @($AdministratorsSid, $SystemSid) -Owner $AdministratorsSid
    # A link left inside by whoever could write here before would carry what
    # this script installs out to where they still can.
    $links = @(Get-ChildItem -LiteralPath $Base -Recurse -Force -Attributes ReparsePoint -ErrorAction SilentlyContinue)
    if ($links.Count -gt 0) {
        throw ("$Base holds links, which could lead what SYSTEM runs somewhere a user can write: " + (($links | ForEach-Object { $_.FullName }) -join ', ') + ". Remove them and run this again.")
    }
    Note "$Base is Administrators' and SYSTEM's alone"
} else {
    Protect-Base -Sids @($UserSid)
    Note ("$Base is " + $identity.Name + "'s alone")
}

if (Test-Path -LiteralPath $OldXmlPath -PathType Leaf) {
    Remove-Item -LiteralPath $OldXmlPath -Force
    Note ("removed " + $OldXmlPath + ", which an older installer left for an elevated import")
}

# ---------------------------------------------------------------------------
# Agent tree
# ---------------------------------------------------------------------------

Step 'Agent'

$SrcAgent = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path (Join-Path $SrcAgent 'src\index.mjs'))) {
    throw "No agent source at $SrcAgent\src\index.mjs. Copy the repo across first."
}
if (-not (Test-Path (Join-Path $SrcAgent 'package.json') -PathType Leaf)) {
    throw "No package metadata at $SrcAgent\package.json."
}
if (-not (Test-Path (Join-Path $PSScriptRoot 'launcher.mjs') -PathType Leaf)) {
    throw "Stable launcher helper missing at $PSScriptRoot\launcher.mjs."
}

function Test-AgentTree([string] $Tree) {
    $verifyCode = @'
import { pathToFileURL } from "node:url";
const verifier = await import(pathToFileURL(process.argv[2]));
const result = verifier.verifyInstalledTree(process.argv[3]);
if (!result.ok) { process.stderr.write(result.error + "\n"); process.exit(1); }
process.stdout.write(result.signed ? `signed ${result.manifest.version}` : "unsigned source checkout");
'@
    $oldLibraryMode = $env:LEGIONCTL_LAUNCHER_LIBRARY
    $env:LEGIONCTL_LAUNCHER_LIBRARY = '1'
    try {
        $result = ($verifyCode | & $NodeExe --input-type=module - (Join-Path $PSScriptRoot 'launcher.mjs') $Tree) -join ' '
    } finally {
        $env:LEGIONCTL_LAUNCHER_LIBRARY = $oldLibraryMode
    }
    if ($LASTEXITCODE -ne 0) { throw "Agent tree verification failed for '$Tree'." }
    return $result
}

function Test-AgentSelfCheck([string] $Tree) {
    $entry = Join-Path $Tree 'src\index.mjs'
    $output = (& $NodeExe $entry version --check) -join ' '
    if ($LASTEXITCODE -ne 0) { throw "The staged agent exited with $LASTEXITCODE during version --check." }
    try { $reply = $output | ConvertFrom-Json } catch { throw "The staged agent returned invalid JSON during version --check: $output" }
    $expected = (Get-Content -LiteralPath (Join-Path $Tree 'package.json') -Raw | ConvertFrom-Json).version
    if ($reply.ok -ne $true -or $reply.agentVersion -ne $expected -or $reply.contract -lt 3 -or $reply.selfTest.ok -ne $true) {
        throw "The staged agent did not pass version --check: $output"
    }
}

$sourceTrust = Test-AgentTree $SrcAgent
Note ("source verification: " + $sourceTrust)

$sameTree = $false
if (Test-Path $AgentDir) {
    $a = (Resolve-Path $SrcAgent).ProviderPath.TrimEnd('\')
    $b = (Resolve-Path $AgentDir).ProviderPath.TrimEnd('\')
    $sameTree = $a.Equals($b, [StringComparison]::OrdinalIgnoreCase)
}

if (-not (Test-Path $BinDir)) { New-Item -ItemType Directory -Path $BinDir -Force | Out-Null }
$moduleTemporary = $LauncherModule + '.new.' + [Guid]::NewGuid().ToString('N')
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'launcher.mjs') -Destination $moduleTemporary -Force
Move-Item -LiteralPath $moduleTemporary -Destination $LauncherModule -Force

$nodeLiteral = "'" + $NodeExe.Replace("'", "''") + "'"
$launcherText = (Get-Content -LiteralPath (Join-Path $PSScriptRoot 'launcher.ps1') -Raw).Replace('@NODE_POWERSHELL@', $nodeLiteral)
$launcherTemporary = $LauncherScript + '.new.' + [Guid]::NewGuid().ToString('N')
[IO.File]::WriteAllText($launcherTemporary, $launcherText, (New-Object Text.UTF8Encoding($false)))
Move-Item -LiteralPath $launcherTemporary -Destination $LauncherScript -Force
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'dispatch.ps1') -Destination (Join-Path $BinDir 'dispatch.ps1') -Force


$Backup = $null
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
    $Stage = Join-Path $Base ('agent.install.new.' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $Stage | Out-Null
    Copy-Item -LiteralPath (Join-Path $SrcAgent 'src') -Destination (Join-Path $Stage 'src') -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $SrcAgent 'install') -Destination (Join-Path $Stage 'install') -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $SrcAgent 'package.json') -Destination (Join-Path $Stage 'package.json') -Force
    $hasManifest = Test-Path (Join-Path $SrcAgent 'MANIFEST.json') -PathType Leaf
    $hasSignature = Test-Path (Join-Path $SrcAgent 'MANIFEST.json.sig') -PathType Leaf
    if ($hasManifest -ne $hasSignature) {
        throw 'Source must carry both MANIFEST.json and MANIFEST.json.sig.'
    }
    if ($hasManifest) {
        Copy-Item -LiteralPath (Join-Path $SrcAgent 'MANIFEST.json') -Destination (Join-Path $Stage 'MANIFEST.json') -Force
        Copy-Item -LiteralPath (Join-Path $SrcAgent 'MANIFEST.json.sig') -Destination (Join-Path $Stage 'MANIFEST.json.sig') -Force
    }
    Test-AgentTree $Stage | Out-Null
    Test-AgentSelfCheck $Stage

    $Backup = Join-Path $Base 'agent.prev'
    & $NodeExe (Join-Path $PSScriptRoot 'promote.mjs') $Base $Stage
    if ($LASTEXITCODE -ne 0) { throw 'Agent promotion failed; all retained trees remain available for recovery.' }
    Note "agent promoted to $AgentDir"
    if (Test-Path -LiteralPath $Backup -PathType Container) { Note "previous agent retained at $Backup" }
}

if (-not (Test-Path $AgentEntry -PathType Leaf)) {
    throw "Agent entry point missing at $AgentEntry."
}


try {
    $finalOutput = (& $LauncherScript version --check) -join ' '
    if ($LASTEXITCODE -ne 0) { throw "launcher exited with $LASTEXITCODE" }
    $finalReply = $finalOutput | ConvertFrom-Json
    if ($finalReply.ok -ne $true -or $finalReply.selfTest.ok -ne $true) { throw 'launcher self-check reply was not successful' }
} catch {
    throw "Installed agent failed its final self-check; the current tree and previous backup are retained for inspection: $($_.Exception.Message)"
}
Note "stable launcher installed at $LauncherScript and passed version --check"

# What this session just wrote belongs to Administrators too. Where policy
# makes whoever creates a file its owner, that is the installing account, and
# an owner may re-permission what it owns without being elevated.
if ($TaskRunsAsSystem) {
    Invoke-IcaclsCommand -Arguments @($Base, '/setowner', ('*' + $AdministratorsSid), '/T', '/C', '/Q')
}

# config.json and state.json are deliberately not written here. The agent falls
# back to its own defaults when they are absent, and stamping a fresh config
# over the top would silently flip auto update back on at every install.
if (Test-Path (Join-Path $Base 'config.json')) {
    Note 'config.json is already there and was left untouched'
} else {
    Note 'no config.json; inert defaults apply (no services, no boot targets, automatic maintenance off)'
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

$TaskRegistered = $false
$taskPath       = '\'
$existing       = $null

if ($SkipScheduler) {
    Note 'scheduled task left untouched by -SkipScheduler'
} else {
Step 'Scheduled task'

$existing = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
$LauncherArguments = '"' + $LauncherModule.Replace('"', '\"') + '" cycle'

if (-not $existing) {

    # --- create it ----------------------------------------------------------

    # A task that runs as this account needs no elevation to register, and the
    # agent only ever touches files and services this account owns.
    Note "'$TaskName' does not exist, creating it"

    $action = New-ScheduledTaskAction -Execute $NodeExe -Argument $LauncherArguments -WorkingDirectory $Base

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
    # here, and the time limit is the cycle's budget: a run that has not
    # finished in 30 minutes is stuck. The agent's OS-backed mutex is released
    # when that process exits, and the next cycle recovers its interrupted
    # operation.
    $settings = New-ScheduledTaskSettingsSet `
        -MultipleInstances IgnoreNew `
        -ExecutionTimeLimit $TimeLimitSpan `
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
    Note ("created '$TaskName': " + $NodeExe + " " + $LauncherArguments + ", every 15 minutes as " + $identity.Name)

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

    $argumentsNode = $doc.CreateElement('Arguments', $TaskNamespace)
    $argumentsNode.InnerText = $LauncherArguments
    $execNode.AppendChild($argumentsNode) | Out-Null

    # A task running as SYSTEM starts in C:\Windows\system32. Point it somewhere
    # the agent owns so anything written relative to the working directory lands
    # sensibly.
    $workingNode = $doc.CreateElement('WorkingDirectory', $TaskNamespace)
    $workingNode.InnerText = $Base
    $execNode.AppendChild($workingNode) | Out-Null

    $actions.AppendChild($execNode) | Out-Null
    Note ("new action:   " + $NodeExe + " " + $LauncherArguments)

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

    # --- overlap policy and time limit ---------------------------------------

    # Settings takes its children in any order (settingsType is an xs:all in the
    # task schema), unlike a trigger, so a setting that is missing is appended.
    $settingsNode = $doc.SelectSingleNode('/t:Task/t:Settings', $ns)
    if (-not $settingsNode) {
        throw "The exported XML for '$TaskName' has no Settings element. Delete the task and rerun this script to have it created from scratch."
    }

    # A cycle that installs and waits for health can outlive a 15 minute window.
    # If the task were allowed to run in parallel, two installs would fight over
    # the same files and wreck the install. IgnoreNew is the only sane policy.
    $policyNode = $settingsNode.SelectSingleNode('t:MultipleInstancesPolicy', $ns)
    if (-not $policyNode) {
        $policyNode = $doc.CreateElement('MultipleInstancesPolicy', $TaskNamespace)
        $policyNode.InnerText = 'IgnoreNew'
        $settingsNode.AppendChild($policyNode) | Out-Null
        Note 'overlap policy was not set, set to IgnoreNew'
    } elseif ($policyNode.InnerText -ne 'IgnoreNew') {
        Note ("overlap policy was " + $policyNode.InnerText + ", set to IgnoreNew")
        $policyNode.InnerText = 'IgnoreNew'
    } else {
        Note 'overlap policy already IgnoreNew'
    }

    # The cycle's budget, as for a new task. A shorter limit stops a cycle that
    # was planned to fit, and a longer one, or the default of 72 hours, lets a
    # stuck run hold the task while IgnoreNew skips every run behind it.
    $limitNode = $settingsNode.SelectSingleNode('t:ExecutionTimeLimit', $ns)
    if (-not $limitNode) {
        $limitNode = $doc.CreateElement('ExecutionTimeLimit', $TaskNamespace)
        $settingsNode.AppendChild($limitNode) | Out-Null
        Note ("time limit:   the default of 72 hours, set to " + $TimeLimit)
    } elseif ($limitNode.InnerText -ne $TimeLimit) {
        Note ("time limit:   " + $limitNode.InnerText + ", set to " + $TimeLimit)
    } else {
        Note ("time limit:   already " + $TimeLimit)
    }
    $limitNode.InnerText = $TimeLimit

    # --- register -----------------------------------------------------------

    # schtasks rather than Register-ScheduledTask, because it imports the XML
    # exactly as Export-ScheduledTask wrote it and says what it rejects. The file
    # goes into the base, which only the task's own principal may write, and is
    # removed as soon as schtasks has read it: there is never a task definition
    # lying around for somebody else to change, or for an admin to import later.
    #
    # Save() honours the encoding in the XML declaration, which Export-ScheduledTask
    # emits as UTF-16. Writing it any other way gives schtasks a file whose bytes
    # disagree with its own header.
    $xmlPath = Join-Path $Base ('task.' + [Guid]::NewGuid().ToString('N') + '.xml')
    $doc.Save($xmlPath)
    try {
        $registered = Invoke-Native -Command 'schtasks.exe' -Arguments @('/Create', '/TN', $FullTaskName, '/XML', $xmlPath, '/F')
    } finally {
        Remove-Item -LiteralPath $xmlPath -Force -ErrorAction SilentlyContinue
    }
    if ($registered.ExitCode -ne 0) {
        Say ''
        Say $registered.Output
        throw "schtasks exited with $($registered.ExitCode) re-registering '$FullTaskName'. Check the task in taskschd.msc before rerunning."
    }
    $TaskRegistered = $true
    Note "task '$FullTaskName' re-registered"
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

    # Checked, not assumed: the base was given to the account the task was to
    # run as, and a task running as anybody else would run what that account
    # can write.
    $expectedSid = if ($TaskRunsAsSystem) { $SystemSid } else { $UserSid }
    $liveSid = if ($liveUser) { Resolve-AccountSid $liveUser.InnerText } else { $null }
    if ($liveSid -ne $expectedSid) {
        throw ("'$TaskName' was registered, but it runs as " + $(if ($liveUser) { $liveUser.InnerText } else { 'an account this script cannot name' }) + " rather than " + $expectedSid + ". Check it in taskschd.msc.")
    }
}
}

# ---------------------------------------------------------------------------
# OpenSSH
# ---------------------------------------------------------------------------

# The shell sshd hands a command to decides how the clients have to quote it:
# cmd when nothing is set, PowerShell when the setup says "shell": "powershell"
# for this system. The agent's doctor reads the same value.
Step 'OpenSSH'

$defaultShell = (Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -ErrorAction SilentlyContinue).DefaultShell
if (-not $defaultShell) {
    Note 'sshd DefaultShell: not set, so ssh runs commands with cmd.exe; this system needs no "shell" in the setup, or "cmd"'
} else {
    $shellName = (Split-Path -Leaf $defaultShell).ToLowerInvariant()
    if ($shellName -eq 'cmd.exe') {
        Note ('sshd DefaultShell: ' + $defaultShell + '; this system needs no "shell" in the setup, or "cmd"')
    } elseif ($shellName -eq 'powershell.exe' -or $shellName -eq 'pwsh.exe') {
        Note ('sshd DefaultShell: ' + $defaultShell + '; set "shell": "powershell" for this system in the setup')
    } else {
        Note ('WARNING: sshd DefaultShell: ' + $defaultShell + '. Clients quote commands for cmd and PowerShell only; set it to cmd.exe or powershell.exe, or remove it')
    }
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

Say ''
Say ('  Agent:  ' + $NodeExe + ' ' + $LauncherModule + ' status')
Say ('  Config: ' + (Join-Path $Base 'config.json'))
Say ('  Task:   Get-ScheduledTaskInfo -TaskName "' + $TaskName + '"')
Say ('  Logs:   ' + (Join-Path $Base 'legionctl.log'))
Say ''

# Exit codes, so a script driving this over SSH can tell the outcomes apart
# instead of reading the prose:
#   0  fully installed, the task now runs the agent, or with -Uninstall removed
#   1  something threw, see the message; a refused principal changed nothing
exit 0

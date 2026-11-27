<#
.SYNOPSIS
  El1xr launcher (Bone/Nevaa): builds (cargo skips the work when current) and launches VIctory (third_party/victory,
  Neptune base) on the user's own game folder.

.PARAMETER GameDir
  Folder of the game package (kept outside this repository). It must contain game_scarlett_master.exe.

.PARAMETER Warm
  Optional. Before the game, runs 'victory.exe warm' on the .rpf archives of the game folder only (never the folder
  itself: a game folder can hold other trees, which warm would walk too). It translates their shaders into the cache
  and makes their compute pipelines, with the same environment variables as the game (the cache key depends on them).
  About 7 minutes the first time; later launches reuse the cache.

.PARAMETER Precompile
  Optional. Before the game, runs 'victory.exe warm' on shaders.rpf, the game executable and its two DLLs
  (XboxBvhBuilder.dll, amd_fidelityfx_upscaler_xbox.dll), with the same environment variables as the game.
  Combine with -Warm to cover every archive as well.

.PARAMETER NoGame
  Optional. Stops after -Warm / -Precompile without starting the title (an offline prewarm: `dev.py prewarm`).

.PARAMETER WarmJobs
  Optional. Worker threads for -Warm / -Precompile (default: every logical core).

.PARAMETER WarmNoPipelines
  Optional. -Warm / -Precompile only translate shaders: no compute pipelines, no device on the card when
  VICTORY_HOST_WAVE and VICTORY_VIEW_LIMIT give the host's facts (as `dev.py prewarm` does).

.PARAMETER WarmRecipes
  Optional. After -Warm / -Precompile, makes the host pipeline of every recipe earlier runs wrote down
  (VICTORY_PIPELINE_RECIPES=1) into the pipeline library (`victory warm --recipes`). Makes a device on the card:
  only under the gpu lock (`dev.py prewarm --pipelines`).

.PARAMETER AutoA
  Optional. Taps A through the start screens for 150 s (VICTORY_AUTO_A=150), as Neptune's play.ps1 does by default.

.PARAMETER FrametimeDump
  Optional. Writes frametime.csv into the cache folder, one row a second (VICTORY_FRAMETIME_DUMP=1).

.PARAMETER Diag
  Optional. Builds and runs the diag profile with the diagnostics feature instead of the release build the frame rates
  were measured with. -Dred implies it.

.PARAMETER SkipPipelines
  Optional. Passed unchanged to VICTORY_SKIP_PIPELINES when given.

.PARAMETER NoTunedSettings
  Optional. Does not set Neptune's switch set below (only VICTORY_GAME_DIR, VICTORY_DISCORD and
  VICTORY_OCEAN_READBACK are set).

.PARAMETER Dred
  Optional. Turns on device-removed data (VICTORY_D3D12_DRED) to diagnose a device loss; needs the diagnostics build,
  so it implies -Diag. Off by default: it notes every command list operation and slows each frame.

.PARAMETER XgDll
  Optional. Path to your own copy of xg_xs.dll, the offline PC build of the GDK's texture layout library (never in this
  repository). VIctory then lays out the title's textures with it (VICTORY_XG_DLL).

.PARAMETER XgMode
  Optional, with -XgDll. 'real' (default) serves the title from xg_xs.dll; 'compare' keeps VIctory's layouts and logs
  every difference with the library (VICTORY_XG_MODE).

.PARAMETER BatchCardRehash
  Optional. Sets VICTORY_CARD_REHASH_RUNS=1 even with -NoTunedSettings (Neptune's switch set already sets it).

.PARAMETER Override
  Optional. A hashtable of VICTORY_* variables set after the tuned set (and after -NoTunedSettings), for A/B runs:
  -Override @{ VICTORY_RELAY = '0'; VICTORY_PIPELINE_WAIT_MS = '12000' }. Applied last, so it wins over the set.

.PARAMETER Quality
  Optional. Applies the picture-first set on top of the tuned set (after -NoTunedSettings too, before -Override):
  windows and buffers are read fresh each frame instead of kept as card copies, with more VRAM headroom. It costs
  about half the frame rate: measured about 7 to 9 fps against 14 to 17 for the default set, with 51 to 93 texture
  refusals against 296 (run auto-ram-2326). Picture first, frame rate second.

.PARAMETER Mute
  Optional. Starts the title with its Windows audio session muted (VICTORY_MUTE=1) without touching XAudio2 or game
  timing. Set before -Override so -Override @{ VICTORY_MUTE = '0' } still wins.

.PARAMETER SkipIntro
  Optional. Skips the title's startup intro movie straight to the initial interactive screen (VICTORY_SKIP_INTRO=1,
  equivalent to prepending -skipToIIS to the title arguments) without affecting story cutscenes. Set before -Override
  so -Override @{ VICTORY_SKIP_INTRO = '0' } still wins.

.PARAMETER DataDir
  Optional. Isolated data directory for saves and persistent local storage (VICTORY_DATA_DIR). Set before -Override so
  -Override @{ VICTORY_DATA_DIR = '...' } still wins.

.PARAMETER TitleArgs
  Optional. Arguments forwarded to the title executable after `--` (e.g. -TitleArgs @('-skipToIIS')).

.NOTES
  The switch set is Neptune (VIctory Edit.) 2026-11-25c's play.ps1 for the Series X path (no PC FidelityFX DLL, no
  VICTORY_AUTO_A unless -AutoA), with the measured changes of 2026-11-25 on top (heap pass on dirty slots, copy
  coalescing, 1536 MB windows, texture room 93 %, idle times 8 s / 2 s / 1 s, 512 MB list pool). Measured by the
  maintainer on 2026-11-25 (RTX 4070 SUPER, i7-13700KF, Series X 4K, release build, after warm): median about 13.8 fps
  in the world (15.6 to 17.0 over the last minute), 72.5 ms a frame. VICTORY_CARD_WINDOWS_MB was sized for a 16 GB card:
  4096 spilled VRAM into system RAM on a 12 GB card; on a smaller card, lower VICTORY_CARD_BUFFERS_MB first.
  Discord Rich Presence stays off (VICTORY_DISCORD=0).
  The ocean's height maps are copied back for the title's CPU (VICTORY_OCEAN_READBACK=1), or boats sit on the sea bed.

.EXAMPLE
  .\tools\lancer_victory.ps1 -GameDir D:\Games\MyPackage -Warm

.EXAMPLE
  .\tools\lancer_victory.ps1 -GameDir D:\Games\MyPackage -XgDll D:\MyGdk\xg_xs.dll
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$GameDir,
    [switch]$Warm,
    [switch]$Precompile,
    [switch]$NoGame,
    [int]$WarmJobs = 0,
    [switch]$WarmNoPipelines,
    [switch]$WarmRecipes,
    [switch]$AutoA,
    [switch]$FrametimeDump,
    [switch]$Diag,
    [string]$SkipPipelines,
    [switch]$NoTunedSettings,
    [switch]$Dred,
    [string]$XgDll,
    [ValidateSet('real', 'compare')][string]$XgMode = 'real',
    [switch]$BatchCardRehash,
    [switch]$Quality,
    [switch]$Mute,
    [switch]$SkipIntro,
    [string]$DataDir,
    [string[]]$TitleArgs = @(),
    [hashtable]$Override = @{}
)

$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$victory = Join-Path $repo 'third_party\victory'
$manifest = Join-Path $victory 'Cargo.toml'

if (-not (Test-Path -LiteralPath $manifest)) {
    throw "third_party/victory/Cargo.toml is missing: this copy of El1xr is incomplete."
}

$gameExe = Join-Path $GameDir 'game_scarlett_master.exe'
if (-not (Test-Path -LiteralPath $gameExe)) {
    throw "game_scarlett_master.exe not found in '$GameDir'."
}

# VIctory is its own Cargo workspace: its target folder is third_party\victory\target
# unless CARGO_TARGET_DIR is set.
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $victory 'target' }
$useDiag = $Diag -or $Dred
if ($useDiag) {
    $profileArgs = @('--profile', 'diag', '-p', 'victory', '--features', 'diagnostics')
    $exe = Join-Path $target 'diag\victory.exe'
} else {
    $profileArgs = @('--release', '-p', 'victory')
    $exe = Join-Path $target 'release\victory.exe'
}

# Always ask cargo: it does nothing when victory.exe is current, and rebuilds it after a pull that changes
# third_party/victory (an old victory.exe would ignore the settings the new tree reads).
Write-Host 'El1xr by Bone/Nevaa, running on VIctory (Marvisak and contributors).'
Write-Host "Building VIctory ($(if ($useDiag) { 'diag profile, diagnostics feature' } else { 'release' }))..."
# Windows PowerShell 5.1 wraps redirected native stderr (`2>`) in NativeCommandError, which
# $ErrorActionPreference = 'Stop' turns into a terminating error on benign warnings; rely on $LASTEXITCODE.
$prevErrorAction = $ErrorActionPreference
try {
    $ErrorActionPreference = 'Continue'
    & cargo build @profileArgs --manifest-path $manifest
} finally {
    $ErrorActionPreference = $prevErrorAction
}
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit code $LASTEXITCODE)." }

# No victory.toml is needed: VIctory reads the game folder from VICTORY_GAME_DIR.
$env:VICTORY_GAME_DIR = $GameDir
# Neptune's Discord Rich Presence stays off (our copy also needs VICTORY_DISCORD=1 to turn it on).
$env:VICTORY_DISCORD = '0'
# The title's CPU reads the ocean height maps its wave passes write on the GPU: boats float on them. Off by default in
# our copy (texback.rs); without it they read memory nothing wrote and boats sit on the sea bed. Linear layouts only
# (VICTORY_TEXTURE_BACK_MODES left unset): the height maps are linear, XOR modes turned the water to foam.
$env:VICTORY_OCEAN_READBACK = '1'
if (-not $NoTunedSettings) {
    # Neptune (VIctory Edit.) 2026-11-25c, play.ps1, Series X path. Its VICTORY_WAVE_OWN_LDS_GROUPS is left out: no code
    # of this tree reads it (Neptune's shader translator, from soldsawn/victory-nvidia-8gb, dropped that wave rule).
    $tuned = [ordered]@{
        VICTORY_CARD_WINDOWS_MB          = '1536'      # draw data on the card; 4096 spilled VRAM on a 12 GB card
        VICTORY_BAKE_TEXTURES_MS         = '500'       # cap decoration/character bake upload to avoid 2s+ mid-gameplay freezes
        VICTORY_EARLY_TEXTURES_MS        = '16'        # tight frame-pacing budget for early placed textures (background prepare handles detile)
        VICTORY_WIDE_MERGE               = '1'         # shader results merged back on several threads
        VICTORY_RELAY_WIDE_MERGE         = '1'         # relay landings merged back on several threads
        VICTORY_RELAY_OWN_QUEUE          = '1'         # copy-back fetches on a GPU queue of their own
        VICTORY_UNDER_WRITES_WORDS       = '1'         # small changes under shader writes do not force a wait
        VICTORY_UNDER_WRITES_RUNS        = '100000'
        VICTORY_UNDER_WRITES_MASKS       = '1'
        VICTORY_RELAY                    = '2'
        VICTORY_WIDE_REFRESH             = '1'
        VICTORY_ASK_ONCE                 = '1'
        VICTORY_TEXTURE_NO_WAIT          = '1'         # do not stall xmem streaming threads waiting on texture remaps
        VICTORY_CARD_REHASH_RUNS         = '1'
        VICTORY_JOIN_COPIES              = '1'
        VICTORY_D3D12_BACKGROUND         = '0'
        VICTORY_LATE_WAIT_LISTS          = '1024'
        VICTORY_WINDOW_TAILS             = '1'
        VICTORY_TEXTURE_STAGE_MB         = '512'
        VICTORY_CARD_BUFFERS_MIN_KB      = '64'
        VICTORY_ASK_QUIET                = '8'         # quiet blocks checked once a frame; recently written blocks every submission
        VICTORY_WATCH_4K                 = '1'         # refresh only written 4 KiB sub-pages instead of whole 64 KiB pages
        VICTORY_COPY_CHANGED             = '1'         # only bytes that differ go up to the card
        VICTORY_QUIET_BACK               = '1'         # copied-back shader results are not read again
        VICTORY_LIST_POOL_MOST           = '2048'      # command lists kept, not made anew each frame
        VICTORY_LIST_POOL_MB             = '512'
        VICTORY_WIDE_THREADS             = '16'
        VICTORY_UNDER_WRITES_MASKS_LEAST = '1'         # small changes go up in one pass per buffer
        VICTORY_INDIRECT_RAW             = '2'         # indirect draws use the game's own arguments
        VICTORY_TEXTURE_HASH_EVERY       = '8'         # re-hash unguarded textures at default cadence instead of every 32 frames
        VICTORY_LEVELS_ON_DEMAND         = '1'         # card memory only for the mip levels the game holds
        VICTORY_VIEWS_REMADE_QUIET       = '0'         # apply mip/LOD view promotions immediately instead of deferring while submissions are in flight
        VICTORY_BVH_NO_HIT               = '1'         # ray tests return clean miss when BVH builder is off: fixes oily skin, lit corners, and GPU cost
        VICTORY_HDR_STANDIN              = '10'        # R16G16B16A16_FLOAT stand-in for R9G9B9E5 (matches victory-fork)
        VICTORY_WAVE_PAIRS               = '0'         # avoid 16x16 blocky tile artifacts from unsynced paired-lane LDS on Volta+
        VICTORY_WAVE_LDS_LOOSE           = '0'         # avoid loose intra-region LDS ordering in wave64 compute shaders
        VICTORY_WINDOW_KB                = '1024'
        VICTORY_CARD_BUFFERS_MB          = '1536'
        VICTORY_CARD_WINDOW_UPLOAD       = '1'
        VICTORY_DROP_DXIL                = '1'
        VICTORY_DESC_DATA                = '1'         # shaders that read a descriptor as data translate
        VICTORY_VRAM_IDLE_MS             = '8000,2000,1000'    # an unused texture is kept 8 s while the card has room
        VICTORY_TEXTURE_ROOM_PERCENT     = '93'        # textures use up to 93 % of the card's room
        VICTORY_HEAP_PASS_DIRTY          = '1'         # the heap pass looks only at slots marked since its last pass
        VICTORY_COPY_COALESCE            = '1024'      # changed runs up to 1024 bytes apart go up as one range
        VICTORY_REAPER                   = '1'         # drop released host D3D12 objects on a background reaper thread
        VICTORY_ASYNC_SUBMIT             = '1'         # submissions prepared on a thread of ours; the title's call returns at once
        VICTORY_GUARD_HOT                = '0'         # keep page guards active on hot view memory blocks
        VICTORY_AMBIENT                  = '0.8'       # ambient diffuse light level (RTX visual fix)
        VICTORY_AMBIENT_AIR              = '1.5'       # ambient air/sky multiplier (RTX visual fix)
        VICTORY_AMBIENT_MOST             = '1.2'       # ambient lighting cap (RTX visual fix)
        VICTORY_NAN_SCRUB                = '1'         # scrub NaN shader outputs (RTX visual fix)
        VICTORY_SHOW_AFTER               = '1'         # present after pending work (RTX visual fix)
        VICTORY_THREAD_TICKET            = '0'         # avoid WAVE_PAIRS=0 + ticket GPU hang on tiled lighting
        VICTORY_HEAP_SCAN_EVERY          = '16'        # scan kept heap slots every 16 passes (+7% FPS, 4x fewer stutters)
        VICTORY_CLAMP_AFTER_WRITER       = '1'         # sanitize indirect args once after writer instead of per-draw barriers
        VICTORY_WAVE32_NVIDIA            = '1'         # NVIDIA native Wave32 lowering (ACCC, active loop, restored lanes)
        VICTORY_REUSE_ROOT_CBV           = '1'         # deduplicate repeated unaligned root CBV staging per list
        VICTORY_TEXTURE_PREPARE_THREADS  = '4'         # prepare & detile streamed textures off the recording thread
        VICTORY_WATCH_RESERVATION        = '1'         # batch GetWriteWatch queries by memory reservation
        VICTORY_PORT_CBV                 = '1'         # bind 16-byte-aligned table CBVs in-place as raw SRVs (skips ~22k scratch copies/frame)
        VICTORY_DIRTY_4K                 = '1'         # narrow write-watched copy spans to dirty 4 KiB subpages (-45% copy bytes)
        VICTORY_PORT_GUARD               = '1'         # concurrent guard lock (GUARDS.read + per-block claims)
        VICTORY_STAGING_UNLOCKED         = '1'         # batch 1: staging buffers built outside the global lock
        VICTORY_PREPARE_TAKE_BY_TABLE    = '1'         # batch 1: a table's take frees the prepare entry at once
        VICTORY_SHADOW_ALLOC_OUTSIDE     = '1'         # batch 1: buffer copies made outside the table lock (Vice-Tay #199)
        VICTORY_LOOSE_ALLOC_OUTSIDE      = '1'         # batch 1: loose copies made outside the table lock (Vice-Tay #238)
        VICTORY_SHADOW_READ_LOCKS        = '1'         # batch 1: read-only table walks take read locks (Vice-Tay #238)
        VICTORY_TABLED_ALL               = '1'         # batch 1: every slot of a bound table marked (fxceless #1414)
        VICTORY_LIST_VIEWS               = '1'         # batch 1: a fresh table's views made once per list (cool_fox)
        VICTORY_BAKE_PARALLEL            = '8'         # batch 1: bake textures created on 8 threads
        VICTORY_BAKE_STAGE_AHEAD         = '1'         # batch 1: bake textures fully prepared ahead
        VICTORY_TRANSLATE_AHEAD          = '4'         # batch 1: shaders translated when the title creates the pipeline
        VICTORY_LIBRARY_NAME_LOCK        = '1'         # batch 1: pipeline library loads lock per name (fxceless #1442)
        VICTORY_LIBRARY_PREFETCH         = '1'         # batch 1: pipeline library read ahead (avatarmanz #443)
        VICTORY_MAP_LEAN                 = '1'         # batch 1: map straight into freed space
        VICTORY_STREAM_PRIORITY          = '1'         # batch 1: title streaming thread above normal (cool_fox)
        VICTORY_REMAP_KEEP               = '1'         # batch 1: a failed remap keeps the old pages (fxceless #1460)
        VICTORY_BUFFER_AHEAD             = '1'         # batch 1: heap-buffers pass makes its host copies ahead
        VICTORY_PRELUDE_RECHECK          = '1'         # batch 1: no stale prelude upload over a baked texture (trustno1 #33)
        VICTORY_SAMPLER_ORDER            = '1'         # batch 1: static samplers in a fixed order (fxceless #1461)
        VICTORY_ALIAS_AFTER_UPLOAD       = '1'         # batch 1: GPU-written texture copy kept after a CPU upload (Vice-Tay #141)
        VICTORY_PREFILL_RETRY            = '1'         # batch 1: cached mips kept when upload room runs out (Vice-Tay)
        VICTORY_CARD_SIDE                = '1'         # batch 3: card-side copies
        VICTORY_SECTION_MB               = '2'         # batch 3: 2 MB sections
        VICTORY_HELPER_PRIORITY          = '1'         # batch 3: helper threads above normal
        VICTORY_DSTORAGE_PRIORITY        = '-1'        # batch 3: DirectStorage thread below normal
        VICTORY_JOINED_APART             = '1'         # batch 3: joined copies kept apart
        VICTORY_SCENE_INPUT              = '1'         # batch 3: scene input
        VICTORY_ALIAS_PINS               = '1'         # batch 3: alias pins
        VICTORY_STAGING_BLOCKS           = '1'         # batch 3: staging from pooled blocks (boot 32 s vs 60 s)
        VICTORY_CARD_BUFFER_BLOCKS       = '1'         # batch 3: card buffers from pooled blocks
        VICTORY_SHARED_BUFFER_BLOCKS     = '1'         # batch 3: shared buffers from pooled blocks
        VICTORY_UPLOAD_BLOCKS            = '1'         # batch 3: uploads from pooled blocks
        VICTORY_GUARD_4K                 = '1'         # batch 3: 4 KiB guards, no crash, neutral
        VICTORY_BAKE_INPUTS              = '1'         # batch 4: face/skin bakes get their own textures first (0 black faces in 24 diner snaps vs 0-3 per run)
        VICTORY_LOAD_FPS                 = '60'        # batch 4: loading screen up to 60 fps until the world (boot -5 s)
        VICTORY_LOAD_END                 = 'world'     # batch 4: loading rate ends at the first world frame
        VICTORY_LOAD_UNPACED             = '1'         # batch 4: loading screen not paced by our clock
        VICTORY_HOLD_LAST_FRAME          = '1'         # batch 4: an all-black present shows the last good frame (40 of 71 hidden)
        VICTORY_MENU                     = '1'         # in-game settings menu: F9 (or View+Menu held 1 s); inert while closed
    }
    foreach ($name in $tuned.Keys) {
        Set-Item -Path "Env:$name" -Value $tuned[$name]
    }
}
if ($Quality) {
    # Picture first: windows and buffers are read fresh each frame instead of kept as card copies (0 = no windows),
    # with more VRAM headroom for the textures. About half the frame rate of the default set.
    $env:VICTORY_CARD_WINDOWS_MB = '0'
    $env:VICTORY_CARD_BUFFERS_MB = '1024'
    $env:VICTORY_LIST_POOL_MOST = '256'
    $env:VICTORY_LIST_POOL_MB = '256'
    $env:VICTORY_TEXTURE_STAGE_MB = '192'
}
# The user's settings file (the in-game menu writes it, %LOCALAPPDATA%\victory\settings.toml; VICTORY_SETTINGS_FILE names
# another, 'none' for no file as dev.py's runs use): its values win over the tuned set and -Quality; the switches below
# and -Override win over it. VIctory reads the same file for what the environment leaves unset.
$settingsFile = if ($env:VICTORY_SETTINGS_FILE) { $env:VICTORY_SETTINGS_FILE } elseif ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA 'victory\settings.toml' } else { '' }
if ($settingsFile -and $settingsFile -ne 'none' -and (Test-Path -LiteralPath $settingsFile -PathType Leaf)) {
    foreach ($line in Get-Content -LiteralPath $settingsFile -Encoding UTF8) {
        if ($line -notmatch '^\s*(VICTORY_[A-Z0-9_]+)\s*=\s*(.*?)\s*$') { continue }
        $name = $Matches[1]
        $value = $Matches[2]
        if ($value -match '^"((?:[^"\\]|\\.)*)"') {
            $value = $Matches[1] -replace '\\(.)', '$1'
        } elseif ($value -match "^'([^']*)'") {
            $value = $Matches[1]
        } else {
            $value = $value -replace '\s*#.*$', ''
            if ($value -eq 'true') { $value = '1' } elseif ($value -eq 'false') { $value = '0' }
        }
        Set-Item -Path "Env:$name" -Value $value
    }
}
if ($Mute) {
    $env:VICTORY_MUTE = '1'
}
if ($SkipIntro) {
    $env:VICTORY_SKIP_INTRO = '1'
}
if ($PSBoundParameters.ContainsKey('DataDir') -and $DataDir) {
    $env:VICTORY_DATA_DIR = $DataDir
}
foreach ($name in $Override.Keys) {
    if ($name -notlike 'VICTORY_*') { throw "-Override takes VICTORY_* names only: '$name'." }
    Set-Item -Path "Env:$name" -Value ([string]$Override[$name])
}
if ($AutoA) {
    $env:VICTORY_AUTO_A = '150'
}
if ($FrametimeDump) {
    $env:VICTORY_FRAMETIME_DUMP = '1'
}
# VIctory turns DRED on when the variable exists, whatever its value: only set it when asked.
if ($Dred) {
    $env:VICTORY_D3D12_DRED = '1'
}
if ($BatchCardRehash) {
    $env:VICTORY_CARD_REHASH_RUNS = '1'
}
if ($PSBoundParameters.ContainsKey('SkipPipelines') -and $SkipPipelines) {
    $env:VICTORY_SKIP_PIPELINES = $SkipPipelines
}
# Your own copy of the GDK's offline PC xg_xs.dll (never in this repository): Microsoft's library lays out the textures.
if ($PSBoundParameters.ContainsKey('XgDll') -and $XgDll) {
    if (-not (Test-Path -LiteralPath $XgDll -PathType Leaf)) {
        throw "xg_xs.dll not found: '$XgDll'."
    }
    $env:VICTORY_XG_DLL = (Resolve-Path -LiteralPath $XgDll).Path
    $env:VICTORY_XG_MODE = $XgMode
}

$warmArgs = @()
if ($WarmJobs -gt 0) { $warmArgs += @('--jobs', [string]$WarmJobs) }
if ($WarmNoPipelines) { $warmArgs += '--no-pipelines' }
if ($WarmRecipes) { $warmArgs += '--recipes' }

if ($Warm) {
    # Same environment as the game below: the cache key depends on it. Only the game folder's own .rpf files.
    $archives = @(Get-ChildItem -LiteralPath $GameDir -Filter '*.rpf' -File | Sort-Object Name | ForEach-Object { $_.FullName })
    if ($archives.Count -eq 0) {
        throw "-Warm found no .rpf archive in '$GameDir'."
    }
    Write-Host "Warming the shader cache from $($archives.Count) archives (about 7 minutes the first time)..."
    $prevErrorAction = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        & $exe warm @warmArgs @archives
    } finally {
        $ErrorActionPreference = $prevErrorAction
    }
    if ($LASTEXITCODE -ne 0) { throw "victory warm failed (exit code $LASTEXITCODE)." }
}

if ($Precompile) {
    $shaders = Join-Path $GameDir 'shaders.rpf'
    $dlls = @('XboxBvhBuilder.dll', 'amd_fidelityfx_upscaler_xbox.dll') | ForEach-Object { Join-Path $GameDir $_ }
    foreach ($needed in @($shaders) + $dlls) {
        if (-not (Test-Path -LiteralPath $needed -PathType Leaf)) {
            throw "-Precompile needs '$needed'."
        }
    }
    Write-Host 'Precompiling the shaders of shaders.rpf, the game and its DLLs into the cache...'
    $prevErrorAction = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        & $exe warm @warmArgs $shaders $gameExe @dlls
    } finally {
        $ErrorActionPreference = $prevErrorAction
    }
    if ($LASTEXITCODE -ne 0) { throw "victory warm failed (exit code $LASTEXITCODE)." }
}

if ($NoGame) {
    exit 0
}

$prevErrorAction = $ErrorActionPreference
try {
    $ErrorActionPreference = 'Continue'
    if ($TitleArgs.Count -gt 0) {
        $titleTail = @('--') + $TitleArgs
        & $exe run $gameExe --execute @titleTail
    } else {
        & $exe run $gameExe --execute
    }
} finally {
    $ErrorActionPreference = $prevErrorAction
}
exit $LASTEXITCODE

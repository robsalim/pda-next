# Ярлыки запуска PDA Next.
#
# Создаёт рядом с exe два ярлыка:
#   «PDA Next.lnk»              — обычный запуск (опрос модулей включён);
#   «PDA Next — просмотр.lnk»   — с ключом --view-only: опрос не запускается,
#                                 порты (в т.ч. Modbus 502) не занимаются,
#                                 доступны история и графики.
#
# Второй ярлык нужен, чтобы рядом с рабочей копией открыть окно только для
# просмотра: две копии не конфликтуют, потому что desktop слушает свой
# эфемерный порт.
#
# Примеры:
#   .\make_shortcut.ps1                     # ярлыки в target\release
#   .\make_shortcut.ps1 -Desktop            # и на рабочий стол
#   .\make_shortcut.ps1 -Exe out\pda-desktop.exe -Dest out

param(
    # Путь к pda-desktop.exe. По умолчанию — свежая release-сборка.
    [string]$Exe = "",
    # Куда положить ярлыки. По умолчанию — папка с exe.
    [string]$Dest = "",
    # Дополнительно положить ярлык «только просмотр» на рабочий стол.
    [switch]$Desktop
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
if (-not $Exe) { $Exe = Join-Path $root 'target\release\pda-desktop.exe' }
if (-not (Test-Path $Exe)) {
    throw "Не найден $Exe. Соберите его: cargo build --release -p pda-desktop"
}
$Exe = (Resolve-Path $Exe).Path
$workDir = Split-Path -Parent $Exe
if (-not $Dest) { $Dest = $workDir }
New-Item -ItemType Directory -Force $Dest | Out-Null
$Dest = (Resolve-Path $Dest).Path

# WebView2Loader.dll обязателен рядом с exe на тулчейне windows-gnu —
# без него окно не откроется, поэтому проверяем и предупреждаем сразу.
if (-not (Test-Path (Join-Path $workDir 'WebView2Loader.dll'))) {
    Write-Warning "Рядом с exe нет WebView2Loader.dll — окно WebView2 не запустится (см. build.rs pda-desktop)."
}

$shell = New-Object -ComObject WScript.Shell

function New-PdaShortcut {
    param([string]$Dir, [string]$Name, [string]$Arguments)

    $path = Join-Path $Dir "$Name.lnk"
    $lnk = $shell.CreateShortcut($path)
    $lnk.TargetPath = $Exe
    $lnk.Arguments = $Arguments
    $lnk.WorkingDirectory = $workDir
    $lnk.IconLocation = "$Exe,0"
    $lnk.Description = if ($Arguments) {
        'PDA Next — только просмотр истории (опрос модулей не запускается)'
    } else {
        'PDA Next — опрос модулей, графики и архив'
    }
    $lnk.Save()
    Write-Host "Ярлык: $path"
}

New-PdaShortcut -Dir $Dest -Name 'PDA Next' -Arguments ''
New-PdaShortcut -Dir $Dest -Name 'PDA Next — просмотр' -Arguments '--view-only'

if ($Desktop) {
    $desk = [Environment]::GetFolderPath('Desktop')
    if ($desk) {
        New-PdaShortcut -Dir $desk -Name 'PDA Next — просмотр' -Arguments '--view-only'
    }
}

$ErrorActionPreference = 'Stop'
$env:RUSTFLAGS='-C instrument-coverage';
& cargo test --test test


$ErrorActionPreference = 'Continue' # ignore the first error where stderr is interpreted

# extract test target
$output = (& "cargo" test --test test --no-run *>&1) | ForEach-Object ToString
$ErrorActionPreference = 'Stop'
$output -match "(target\\.*\.exe)";
$target = $Matches[0]
Write-Output "using target ${target}"

& llvm-cov report `
  --use-color --ignore-filename-regex='\\.cargo\\registry' `
  --instr-profile=winio.profdata `
  --object "${target}"
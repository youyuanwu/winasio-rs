cmake_minimum_required(VERSION 3.14)

# cmake only includes targets to do coverage
project(winasio-rs LANGUAGES)

find_program(cargo_exe cargo
REQUIRED
)

# `-- --quiet` puts libtest in terse mode (a dot per test) for these targets.
#
# It is not about noise. The Visual Studio generator runs these commands through
# MSBuild's CustomBuild task, which scans their stdout for the canonical
# diagnostic format `origin : category code : text` and fails the task when it
# thinks it saw an error -- regardless of the command's own exit code.
#
# libtest prints one line per test as `test <module path>::<name> ... ok`. A
# crate with a top-level module named `error` therefore emits
#
#     test error::tests::a_transport_error_keeps_the_platform_code_reachable ... ok
#
# which MSBuild reads as origin `test`, category `error`, and logs as a build
# error. `cargo llvm-cov` then exits 0, writes lcov.info, and MSBuild still
# reports `MSB8066 ... exited with code -1`. Measured, not guessed: this is
# precisely how winasio-util's `error` module broke the coverage step while
# every one of its tests passed. winasio escaped it only by accident, because
# its error modules are nested (`winhttp::error::tests::...`) and so do not
# start the line with the word `error`.
#
# Renaming the offending module was rejected as a fix: it leaves the trap armed
# for the next test whose path happens to begin with `error` or `warning`, and
# a test name should never be able to fail a build. Terse output removes the
# per-test lines entirely, so no test name can ever reach the parser. Failures
# still print in full -- and a real test failure fails the build through the
# exit code, which is the mechanism that should decide this. Per-test names
# remain available from the dedicated `cargo test` CI step.
add_custom_target(cov_all
  COMMAND ${cargo_exe} llvm-cov --workspace --all-features --lcov --output-path lcov.info -- --quiet
  WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
)

add_custom_target(cov_summary
  COMMAND ${cargo_exe} llvm-cov --workspace --all-features --no-clean -- --quiet
  WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
)
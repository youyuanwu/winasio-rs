cmake_minimum_required(VERSION 3.14)

# cmake only includes targets to do coverage
project(winasio-rs LANGUAGES)

find_program(cargo_exe cargo
REQUIRED
)

add_custom_target(cov_all
  COMMAND ${cargo_exe} llvm-cov --lcov --output-path lcov.info
  WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
)

add_custom_target(cov_summary
  COMMAND ${cargo_exe} llvm-cov --no-clean
  WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
)
# SPDX-License-Identifier: MIT
#
# Compiler of the pinned llama.cpp runtime for x86_64-pc-windows-msvc.
# `.cargo/config.toml` hands this file and the Ninja generator to the build
# script of `llama-cpp-sys-2` for that target. No other target reads it.
#
# Every machine compiles the runtime with clang-cl. Its matrix kernel
# (`tinyBLAS::gemm_bloc` in ggml-cpu/llamafile/sgemm.cpp) accumulates into a
# 3x4 array of AVX2 vectors. MSVC 19.44 of Visual Studio 2022 leaves that
# array in memory and loads and stores every vector in each step. MSVC 19.51
# of Visual Studio 2026 and clang keep the vectors in registers. A scoring
# request of the bundled model took about 40 % longer with 19.44, so the
# Visual Studio that happens to be installed must not choose the compiler.
#
# The Microsoft headers, libraries and linker are still the ones `cc` finds;
# the build script passes their environment to CMake.

file(TO_CMAKE_PATH "$ENV{ProgramFiles}" _kanade_program_files)
set(_kanade_x86 "ProgramFiles(x86)")
file(TO_CMAKE_PATH "$ENV{${_kanade_x86}}" _kanade_program_files_x86)
# Visual Studio can install its own copies of both tools.
file(GLOB _kanade_visual_studio
  "${_kanade_program_files}/Microsoft Visual Studio/*/*"
  "${_kanade_program_files_x86}/Microsoft Visual Studio/*/*")
set(_kanade_vs_llvm "")
set(_kanade_vs_ninja "")
foreach(_kanade_instance IN LISTS _kanade_visual_studio)
  list(APPEND _kanade_vs_llvm "${_kanade_instance}/VC/Tools/Llvm/x64/bin")
  list(APPEND _kanade_vs_ninja
    "${_kanade_instance}/Common7/IDE/CommonExtensions/Microsoft/CMake/Ninja")
endforeach()

# The LLVM that provides libclang for the bindings also has clang-cl.
find_program(KANADE_LLAMA_CLANG_CL
  NAMES clang-cl
  HINTS ENV LIBCLANG_PATH "${_kanade_program_files}/LLVM/bin"
  PATHS ${_kanade_vs_llvm}
  DOC "clang-cl that compiles the llama.cpp runtime")
if(NOT KANADE_LLAMA_CLANG_CL)
  message(FATAL_ERROR
    "clang-cl was not found. The llama.cpp runtime of Kanade is compiled with "
    "clang-cl on Windows. Install LLVM and put its bin directory on PATH or "
    "in LIBCLANG_PATH.")
endif()

# A CMake that Visual Studio installed has Ninja beside it.
get_filename_component(_kanade_cmake_bin "${CMAKE_COMMAND}" DIRECTORY)
find_program(KANADE_LLAMA_NINJA
  NAMES ninja
  HINTS "${_kanade_cmake_bin}/../../Ninja"
  PATHS ${_kanade_vs_ninja}
  DOC "Ninja that builds the llama.cpp runtime")
if(NOT KANADE_LLAMA_NINJA)
  message(FATAL_ERROR
    "Ninja was not found. The llama.cpp runtime of Kanade is built with Ninja "
    "on Windows. Put ninja on PATH, or install the C++ CMake tools of Visual "
    "Studio.")
endif()
set(CMAKE_MAKE_PROGRAM "${KANADE_LLAMA_NINJA}" CACHE FILEPATH
  "Ninja that builds the llama.cpp runtime" FORCE)

set(CMAKE_C_COMPILER "${KANADE_LLAMA_CLANG_CL}")
set(CMAKE_CXX_COMPILER "${KANADE_LLAMA_CLANG_CL}")

# The build script replaces CMAKE_<LANG>_FLAGS. It leaves the flags of the
# build configuration alone under Ninja, so these reach every compilation.
#
# -ffp-contract=off: clang fuses a multiplication and the addition that
#   follows it into one rounding step. MSVC does not, and the scores of a
#   fused build differ from those the model was measured with. Unfused, the
#   scores equal those of an MSVC build bit for bit.
# -enable-misched=false: the kernel uses all sixteen vector registers: twelve
#   accumulators, three columns and one row. LLVM's instruction scheduler
#   moves the load of the next row above the last use of the current one.
#   Seventeen vectors are then alive, and one accumulator goes through the
#   stack in every step. With the instructions left in source order all
#   twelve stay in registers, which took a tenth off the scoring time.
# /EHsc: C++ exceptions that unwind. This is CMake's default, which the
#   replaced flags dropped. clang-cl refuses `try` and `throw` without it.
set(_kanade_flags "/clang:-ffp-contract=off /clang:-mllvm /clang:-enable-misched=false")
foreach(_kanade_config DEBUG RELEASE RELWITHDEBINFO MINSIZEREL)
  set(CMAKE_C_FLAGS_${_kanade_config}_INIT "${_kanade_flags}")
  set(CMAKE_CXX_FLAGS_${_kanade_config}_INIT "/EHsc ${_kanade_flags}")
endforeach()

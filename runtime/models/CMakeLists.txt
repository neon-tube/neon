# CBMC verification models for the Neon runtime.
#
# Each model is a directory holding `main.c` — a harness that drives some part of
# the runtime through every state it can reach, asserting the properties that must
# hold. CBMC is the compiler and the checker; these are never built with `cc`.
#
#   cmake -B build -S .
#   cmake --build build --target verify-lifecycle
#   cmake --build build --target verify-all
#
# Adding a model means adding a directory. Nothing here needs editing: the glob
# below finds it. That matters for more than convenience — several people can add
# models at once without contending on one file.
#
# ---- Models verify the shipping source, never a copy ----
#
# A model names the runtime `.c` files it needs in `sources.txt`, one relative
# path per line, and CBMC compiles those alongside the harness. It does NOT
# reimplement or paste in the code under test.
#
# This is the whole design, and it is worth stating plainly because the obvious
# shortcut is to inline a copy of the function so CBMC does not have to swallow
# the runtime. A copy proves a property about source that no longer exists the
# moment the real function changes, and nothing tells you it has drifted — the
# model keeps passing. The runtime is one translation unit per area precisely so
# that a model can take the two files it cares about and leave the rest.

cmake_minimum_required(VERSION 3.15)
project(neon_cbmc_models C)

# Checks worth having on by default. `--malloc-may-fail`/`--malloc-fail-null`
# matter more here than they look: every allocation path in the runtime has an
# out-of-memory branch that no test exercises, and CBMC will take it.
set(CBMC_ARGS
    --bounds-check
    --pointer-check
    --pointer-overflow-check
    --memory-leak-check
    --malloc-may-fail
    --malloc-fail-null
    --div-by-zero-check
    --conversion-check
    --unsigned-overflow-check
    --signed-overflow-check
    --unwinding-assertions
    --drop-unused-functions
    --trace
)

# How far loops are unwound. Models should be written so their loops are bounded
# well under this, and `--unwinding-assertions` turns "I guessed too low" into a
# failure rather than a silently partial proof.
set(CBMC_UNWIND 12 CACHE STRING "CBMC loop unwind bound")

find_program(CBMC_EXECUTABLE cbmc)
if(NOT CBMC_EXECUTABLE)
    message(WARNING
        "CBMC not found; verification targets will not be available. "
        "Install it from https://www.cprover.org/cbmc/ or your package manager.")
    return()
endif()

set(RUNTIME_ROOT ${CMAKE_CURRENT_SOURCE_DIR}/..)

# add_cbmc_model(<name>)
#
# Registers `verify-<name>` for the model in `<name>/main.c`. If `<name>/sources.txt`
# exists, each line is a runtime source path relative to `runtime/`, and those are
# compiled with the harness.
function(add_cbmc_model name)
    set(dir ${CMAKE_CURRENT_SOURCE_DIR}/${name})
    set(inputs ${dir}/main.c)

    set(sources_list ${dir}/sources.txt)
    if(EXISTS ${sources_list})
        file(STRINGS ${sources_list} declared)
        foreach(src ${declared})
            string(STRIP "${src}" src)
            if(src STREQUAL "" OR src MATCHES "^#")
                continue()
            endif()
            if(NOT EXISTS ${RUNTIME_ROOT}/${src})
                message(FATAL_ERROR
                    "model '${name}': sources.txt names ${src}, which does not exist. "
                    "A model that silently loses the source it verifies proves nothing.")
            endif()
            list(APPEND inputs ${RUNTIME_ROOT}/${src})
        endforeach()
    endif()

    # ---- optional per-model check opt-out ----
    #
    # A model may name default checks it must run WITHOUT, one flag per line, in
    # `<name>/checks-off.txt`. Each named flag is removed from CBMC_ARGS for this model only.
    #
    # This exists for one honest reason: a check like `--conversion-check` or
    # `--unsigned-overflow-check` flags the runtime's *intentional* well-defined narrowing
    # and two's-complement wrapping -- `neon_str_cmp`'s `(unsigned char)` cast, `neon_i64_add`'s
    # unsigned round-trip -- as if they were bugs. For those functions the check is the wrong
    # oracle, not a caught defect, and no `--no-...` form exists to disable it (CBMC has none
    # for these two), so the model has to run without it. The opt-out lives in the model's
    # own directory, visible like sources.txt, and is a hole in the proof exactly as an
    # ASSUME is (rule 5): the model's header must justify every check it drops. A listed flag
    # that is not actually a default is a hard error, so the file cannot rot into a no-op.
    set(model_args ${CBMC_ARGS})
    set(checks_off ${dir}/checks-off.txt)
    if(EXISTS ${checks_off})
        file(STRINGS ${checks_off} off_flags)
        foreach(flag ${off_flags})
            string(STRIP "${flag}" flag)
            if(flag STREQUAL "" OR flag MATCHES "^#")
                continue()
            endif()
            if(NOT flag IN_LIST model_args)
                message(FATAL_ERROR
                    "model '${name}': checks-off.txt names '${flag}', which is not one of "
                    "the default CBMC_ARGS. A model cannot opt out of a check that is not on, "
                    "and a stale opt-out that quietly does nothing is worse than none.")
            endif()
            list(REMOVE_ITEM model_args ${flag})
        endforeach()
    endif()

    add_custom_target(verify-${name}
        COMMAND ${CBMC_EXECUTABLE}
                ${inputs}
                -I ${RUNTIME_ROOT}/include
                --unwind ${CBMC_UNWIND}
                ${model_args}
        WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}
        DEPENDS ${inputs}
        COMMENT "CBMC: ${name}"
        VERBATIM
    )
    set_property(TARGET verify-${name} PROPERTY SOURCES ${inputs})
endfunction()

add_custom_target(verify-all COMMENT "Run every CBMC model")

file(GLOB model_mains RELATIVE ${CMAKE_CURRENT_SOURCE_DIR}
     ${CMAKE_CURRENT_SOURCE_DIR}/*/main.c)

if(NOT model_mains)
    message(WARNING "no models found under ${CMAKE_CURRENT_SOURCE_DIR}")
endif()

foreach(model_main ${model_mains})
    get_filename_component(model_name ${model_main} DIRECTORY)
    add_cbmc_model(${model_name})
    add_dependencies(verify-all verify-${model_name})
endforeach()

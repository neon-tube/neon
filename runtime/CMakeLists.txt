cmake_minimum_required(VERSION 3.15)
project(neon_rt C)

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)

add_library(neon_rt STATIC
    src/rt.c
)
target_include_directories(neon_rt PUBLIC include)
target_compile_options(neon_rt PRIVATE -Wall -Wextra -Werror)

install(TARGETS neon_rt ARCHIVE DESTINATION lib)
install(DIRECTORY include/ DESTINATION include)

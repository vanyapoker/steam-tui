cmake_minimum_required(VERSION 3.15)
project(steam_tui_transport C CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(BUILD_SHARED_LIB OFF CACHE BOOL "" FORCE)
set(BUILD_STATIC_LIB ON CACHE BOOL "" FORCE)
set(ENABLE_ICE ON CACHE BOOL "" FORCE)
find_package(ZLIB REQUIRED)
add_subdirectory(../../../vendor/GameNetworkingSockets gns)
target_compile_definitions(GameNetworkingSockets_s PRIVATE STEAM_TUI_SDR)
target_include_directories(GameNetworkingSockets_s PRIVATE ${CMAKE_CURRENT_SOURCE_DIR})
find_package(Protobuf CONFIG REQUIRED)
set(SDR_PROTO_SOURCES)
foreach(NAME sdr auth)
  set(PROTO steam_tui_${NAME})
  add_custom_command(OUTPUT ${CMAKE_CURRENT_BINARY_DIR}/${PROTO}.pb.cc ${CMAKE_CURRENT_BINARY_DIR}/${PROTO}.pb.h
    COMMAND protobuf::protoc --cpp_out=${CMAKE_CURRENT_BINARY_DIR}
      -I${CMAKE_CURRENT_SOURCE_DIR} -I${CMAKE_CURRENT_SOURCE_DIR}/../../../vendor/GameNetworkingSockets/src/common
      ${CMAKE_CURRENT_SOURCE_DIR}/${PROTO}.proto
    DEPENDS ${PROTO}.proto protobuf::protoc)
  list(APPEND SDR_PROTO_SOURCES ${CMAKE_CURRENT_BINARY_DIR}/${PROTO}.pb.cc)
endforeach()
add_library(steam_tui_transport STATIC transport.cpp sdr.cpp ${SDR_PROTO_SOURCES})
target_compile_options(steam_tui_transport PRIVATE -fno-rtti)
target_link_libraries(steam_tui_transport PRIVATE GameNetworkingSockets_s)
target_include_directories(steam_tui_transport PRIVATE
  ${CMAKE_CURRENT_BINARY_DIR}
  ../../../vendor/GameNetworkingSockets/src
  ../../../vendor/GameNetworkingSockets/src/common
  ../../../vendor/GameNetworkingSockets/src/public
  ${CMAKE_CURRENT_BINARY_DIR}/gns/src)
target_compile_definitions(steam_tui_transport PRIVATE STEAMNETWORKINGSOCKETS_OPENSOURCE STEAMNETWORKINGSOCKETS_ENABLE_ICE VALVE_CRYPTO_ENABLE_25519 VALVE_CRYPTO_25519_OPENSSLEVP VALVE_CRYPTO_OPENSSL)
add_executable(steam_tui_sdr_tests EXCLUDE_FROM_ALL sdr_tests.cpp)
target_link_libraries(steam_tui_sdr_tests PRIVATE steam_tui_transport GameNetworkingSockets_s protobuf::libprotobuf)
target_include_directories(steam_tui_sdr_tests PRIVATE ${CMAKE_CURRENT_BINARY_DIR} ../../../vendor/GameNetworkingSockets/src/common ${CMAKE_CURRENT_BINARY_DIR}/gns/src)
target_compile_definitions(steam_tui_sdr_tests PRIVATE SDR_FIXTURE_PATH="${CMAKE_CURRENT_SOURCE_DIR}/../../../docs/relay/router-ping-reply.hex")

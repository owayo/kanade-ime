// SPDX-License-Identifier: MIT
#include <array>
#include <atomic>
#include <cstdint>
#include <cstdio>
#include <memory>
#include <string>
#include <thread>
#include <utility>
#ifdef _WIN32
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <fcntl.h>
#include <io.h>
#endif
#include "base/file_stream.h"
#include "base/system_util.h"
#include "data_manager/oss/oss_data_manager.h"
#include "dictionary/dictionary_interface.h"
#include "engine/engine.h"
#include "engine/modules.h"
#include "protocol/commands.pb.h"
#include "protocol/user_dictionary_storage.pb.h"
#include "session/session_handler.h"

namespace {
constexpr uint32_t kMaxFrame = 1u << 20;
constexpr char kControl[] = "\0KANADE\1";
constexpr size_t kControlSize = sizeof(kControl) - 1;

// Dictionary-only reloads never reload or overwrite conversion/prediction learning.
// The worker loop remains available while the replacement token index is built.
class DictionaryReloader {
 public:
  DictionaryReloader(mozc::dictionary::UserDictionaryInterface& dictionary,
                     std::string profile)
      : dictionary_(dictionary), filename_(std::move(profile) + "/user_dictionary.db") {}
  ~DictionaryReloader() { if (thread_.joinable()) thread_.join(); }

  std::string Handle(uint8_t operation, uint64_t generation) {
    uint8_t state = state_.load(std::memory_order_acquire);
    if (operation == 1 && generation != 0 && generation != generation_) {
      if (state == 1) return Reply(4);  // Single-flight: another generation is loading.
      if (thread_.joinable()) thread_.join();
      generation_ = generation;
      state_.store(1, std::memory_order_release);
      try {
        thread_ = std::thread([this] {
          // Finish the initial upstream reloader before publishing our replacement.
          dictionary_.WaitForReloader();
          bool loaded = false;
          try { loaded = Load(); } catch (...) {}
          state_.store(loaded ? 2 : 3, std::memory_order_release);
        });
      } catch (...) {
        state_.store(3, std::memory_order_release);
      }
    }
    return Reply(state_.load(std::memory_order_acquire));
  }

 private:
  bool Load() {
    mozc::InputFileStream input(filename_, std::ios::binary);
    if (!input) return false;
    std::string bytes;
    std::array<char, 8192> buffer{};
    while (input) {
      input.read(buffer.data(), buffer.size());
      const size_t n = static_cast<size_t>(input.gcount());
      if (bytes.size() + n > 8u * 1024 * 1024) return false;
      bytes.append(buffer.data(), n);
    }
    if (!input.eof()) return false;
    mozc::user_dictionary::UserDictionaryStorage storage;
    return storage.ParseFromString(bytes) && dictionary_.Load(storage);
  }
  std::string Reply(uint8_t state) const {
    std::string reply(kControl, kControlSize);
    reply.push_back(static_cast<char>(state));
    for (int i = 0; i < 8; ++i) reply.push_back(static_cast<char>(generation_ >> (i * 8)));
    return reply;
  }
  mozc::dictionary::UserDictionaryInterface& dictionary_;
  const std::string filename_;
  std::thread thread_;
  std::atomic<uint8_t> state_{0};
  uint64_t generation_ = 0;
};
bool ReadFrame(std::string* body) {
  std::array<unsigned char, 4> h{};
  if (std::fread(h.data(), 1, h.size(), stdin) != h.size()) return false;
  const uint32_t n = uint32_t{h[0]} | (uint32_t{h[1]} << 8) |
                     (uint32_t{h[2]} << 16) | (uint32_t{h[3]} << 24);
  if (n == 0 || n > kMaxFrame) return false;
  body->resize(n);
  return std::fread(body->data(), 1, n, stdin) == n;
}
bool WriteFrame(const std::string& body) {
  if (body.empty() || body.size() > kMaxFrame) return false;
  const auto n = static_cast<uint32_t>(body.size());
  const std::array<unsigned char, 4> h{
      static_cast<unsigned char>(n), static_cast<unsigned char>(n >> 8),
      static_cast<unsigned char>(n >> 16), static_cast<unsigned char>(n >> 24)};
  return std::fwrite(h.data(), 1, h.size(), stdout) == h.size() &&
         std::fwrite(body.data(), 1, body.size(), stdout) == body.size() &&
         std::fflush(stdout) == 0;
}
}  // namespace

int RunWorker(const std::string& profile) {
#ifdef _WIN32
  if (_setmode(_fileno(stdin), _O_BINARY) == -1 ||
      _setmode(_fileno(stdout), _O_BINARY) == -1) return 2;
#endif
  mozc::SystemUtil::SetUserProfileDirectory(profile);
  auto modules = mozc::engine::Modules::Create(std::make_unique<mozc::oss::OssDataManager>());
  if (!modules.ok()) return 3;
  auto& dictionary = (*modules)->GetUserDictionary();
  auto engine = mozc::Engine::CreateEngine(*std::move(modules));
  if (!engine.ok()) return 3;
  mozc::SessionHandler handler(*std::move(engine));
  if (!handler.IsAvailable()) return 4;
  DictionaryReloader reloader(dictionary, profile);
  std::string input, output;
  mozc::commands::Command command;
  while (ReadFrame(&input)) {
    if (input[0] == '\0') {
      if (input.size() != kControlSize + 9 || input.compare(0, kControlSize, kControl, kControlSize) != 0) return 5;
      const uint8_t operation = static_cast<uint8_t>(input[kControlSize]);
      if (operation != 1 && operation != 2) return 5;
      uint64_t generation = 0;
      for (int i = 0; i < 8; ++i) generation |= uint64_t{static_cast<unsigned char>(input[kControlSize + 1 + i])} << (i * 8);
      if (!WriteFrame(reloader.Handle(operation, generation))) return 6;
      continue;
    }
    command.Clear();
    if (!command.mutable_input()->ParseFromString(input)) return 5;
    handler.EvalCommand(&command);
    output.clear();
    if (!command.output().SerializeToString(&output) || !WriteFrame(output)) return 6;
    if (command.input().type() == mozc::commands::Input::SHUTDOWN) break;
  }
  return 0;
}

#ifdef _WIN32
int wmain(int argc, wchar_t** argv) {
  if (argc != 2) return 2;
  const int size = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, argv[1],
                                      -1, nullptr, 0, nullptr, nullptr);
  if (size <= 1) return 2;
  std::string profile(size, '\0');
  if (WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, argv[1], -1,
                          profile.data(), size, nullptr, nullptr) != size) return 2;
  profile.pop_back();
  return RunWorker(profile);
}
#else
int main(int argc, char** argv) {
  return argc == 2 ? RunWorker(argv[1]) : 2;
}
#endif

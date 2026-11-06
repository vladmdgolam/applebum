// One AirPlay session on its own thread: audio goes in through a ring, and the
// vendored sender does the rest. The HomePod route needs no PIN (HAP transient).
#include "raop_loop.h"
#include "raop_sender.h"
#include "ring_buffer.h"

#include <algorithm>
#include <atomic>
#include <cstdint>
#include <span>
#include <string>
#include <thread>
#include <vector>

using namespace fxchain;

extern "C" {
// kind: 1 streaming, 0 could not start, 2 the session ended by itself.
typedef void (*AMAirPlayEvent)(void *context, int32_t kind, const char *message);
}

namespace {
struct Session {
    RingBuffer<int16_t> ring{1u << 18};   // about 3 s of stereo at 44.1 kHz
    std::atomic<bool> stop{false};
    std::atomic<double> volume{-1.0};
    std::thread thread;
    std::vector<int16_t> scratch;
};
}

extern "C" void *am_airplay_start(const char *host, uint16_t port, const char *name,
                                  void *context, AMAirPlayEvent event) {
    auto *session = new Session();
    session->thread = std::thread([session, host = std::string(host), port, name = std::string(name), context, event] {
        std::atomic<bool> done{false};
        bool streaming = false;
        RaopLoop loop;
        RaopEvents events;
        events.launched = [&](bool ok, const std::string &error) {
            if (ok) {
                streaming = true;
                event(context, 1, "");
            } else {
                done = true;
                event(context, 0, error.c_str());
            }
        };
        // Only fires when the session ended without us asking.
        events.closed = [&] {
            if (!done.exchange(true)) event(context, streaming ? 2 : 0, "the speaker closed the session");
        };
        events.pinRequired = [&](const std::string &) {
            if (!done.exchange(true)) event(context, 0, "this speaker asks for a PIN, which isn't supported");
        };
        RaopSender sender(loop, std::move(events), RaopLogSink{});
        sender.setInputFormat(44100);
        sender.attachRing(&session->ring);
        sender.setStrictReceiverAuth(true);
        RaopIdentity identity;
        identity.name = "Applebum";
        sender.setIdentity(identity);
        sender.setAuth(RaopDeviceInfo::Auth::HapTransient, true, host, std::string(), std::string());
        sender.start(host, port, name);
        while (!done.load() && !session->stop.load()) {
            const double volume = session->volume.exchange(-1.0);
            if (volume >= 0.0) sender.setVolume(volume);
            loop.pump(sender);
        }
        done = true;
        sender.stop();   // TEARDOWN, flushed before the socket closes
    });
    return session;
}

extern "C" void am_airplay_push(void *handle, const float *samples, size_t count) {
    auto *session = static_cast<Session *>(handle);
    if (session->ring.availableWrite() < count) return;   // the speaker fell behind
    session->scratch.resize(count);
    for (size_t i = 0; i < count; ++i)
        session->scratch[i] = static_cast<int16_t>(std::clamp(samples[i], -1.0f, 1.0f) * 32767.0f);
    session->ring.tryPush(std::span<const int16_t>(session->scratch.data(), count));
}

extern "C" void am_airplay_volume(void *handle, double percent) {
    static_cast<Session *>(handle)->volume.store(percent);
}

extern "C" void am_airplay_stop(void *handle) {
    auto *session = static_cast<Session *>(handle);
    session->stop.store(true);
    if (session->thread.joinable()) session->thread.join();
    delete session;
}

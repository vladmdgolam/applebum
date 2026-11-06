// The only Swift in the app: MusicKit and the audio tap, behind a C ABI.
// Rust owns all state; this file answers calls and reports what the player does.
import AudioToolbox
import CoreAudio
import Foundation
import MusicKit
import Network

public typealias AMReply = @convention(c) (UnsafeMutableRawPointer?, Int32, UnsafePointer<UInt8>?, Int) -> Void
public typealias AMAudio = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<Float>?, Int, UInt32) -> Void

/// A caller's context and callback, carried across a Task. Rust owns the context.
private struct Pending: @unchecked Sendable {
  let context: UnsafeMutableRawPointer?
  let reply: AMReply

  func send(_ status: Int32, _ data: Data = Data()) {
    data.withUnsafeBytes { bytes in
      reply(context, status, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count)
    }
  }
  func send(_ status: Int32, _ text: String) { send(status, Data(text.utf8)) }
  func sendJSON(_ object: [String: Any]) {
    let data = (try? JSONSerialization.data(withJSONObject: object)) ?? Data("{}".utf8)
    send(200, data)
  }
}

// MARK: - Songs already seen

/// Every song an API response has shown. A song on screen can start without
/// another round trip to Apple, which is most of what a song switch waits for.
@MainActor private var knownSongs: [String: Song] = [:]

private func collectResources(_ value: Any, into found: inout [[String: Any]]) {
  if let list = value as? [Any] {
    for item in list { collectResources(item, into: &found) }
  } else if let object = value as? [String: Any] {
    if let type = object["type"] as? String, type == "songs" || type == "library-songs", object["attributes"] != nil {
      found.append(object)
    }
    for item in object.values { collectResources(item, into: &found) }
  }
}

/// Decodes the songs in a response, off the main thread, and remembers them.
private func harvestSongs(from data: Data) {
  Task.detached(priority: .utility) {
    guard let root = try? JSONSerialization.jsonObject(with: data) else { return }
    var resources: [[String: Any]] = []
    collectResources(root, into: &resources)
    guard !resources.isEmpty,
      let collection = try? JSONSerialization.data(withJSONObject: ["data": resources]),
      let songs = try? JSONDecoder().decode(MusicItemCollection<Song>.self, from: collection)
    else { return }
    await MainActor.run {
      for song in songs { knownSongs[song.id.rawValue] = song }
    }
  }
}

/// The songs for these IDs, in order, asking Apple only for those not seen yet.
///
/// A library song can name a catalog ID that the catalog doesn't return, as an
/// upload can. `libraryIDs` maps those catalog IDs to the song's own library ID.
@MainActor private func resolveSongs(_ ids: [String], libraryIDs: [String: String] = [:]) async throws -> [Song] {
  let missing = ids.filter { knownSongs[$0] == nil }
  let library = missing.filter { $0.hasPrefix("i.") }
  let catalog = missing.filter { !$0.hasPrefix("i.") }
  if !catalog.isEmpty {
    var request = MusicCatalogResourceRequest<Song>(matching: \.id, memberOf: catalog.map { MusicItemID($0) })
    request.limit = catalog.count
    for song in try await request.response().items { knownSongs[song.id.rawValue] = song }
  }
  if !library.isEmpty {
    var request = MusicLibraryRequest<Song>()
    request.filter(matching: \.id, memberOf: library.map { MusicItemID($0) })
    request.limit = library.count
    for song in try await request.response().items { knownSongs[song.id.rawValue] = song }
  }
  let fallbacks = ids.filter { knownSongs[$0] == nil }.compactMap { id in libraryIDs[id].map { (id, $0) } }
  if !fallbacks.isEmpty {
    var request = MusicLibraryRequest<Song>()
    request.filter(matching: \.id, memberOf: fallbacks.map { MusicItemID($0.1) })
    request.limit = fallbacks.count
    let found = try await request.response().items
    for (id, libraryID) in fallbacks {
      if let song = found.first(where: { $0.id.rawValue == libraryID }) { knownSongs[id] = song }
    }
  }
  return ids.compactMap { knownSongs[$0] }
}

// MARK: - Authorization and the Apple Music API

@_cdecl("am_authorize")
public func amAuthorize(_ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  Task { @MainActor in
    let status = await MusicAuthorization.request()
    pending.send(status == .authorized ? 200 : 403, "\(status)")
  }
}

/// GET against api.music.apple.com. `{storefront}` in the path is the user's country.
@_cdecl("am_request")
public func amRequest(_ path: UnsafePointer<CChar>, _ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  let template = String(cString: path)
  Task {
    do {
      let country = try await MusicDataRequest.currentCountryCode
      let resolved = template.replacingOccurrences(of: "{storefront}", with: country)
      guard let url = URL(string: "https://api.music.apple.com" + resolved) else {
        pending.send(-1, "bad path: \(resolved)")
        return
      }
      let response = try await MusicDataRequest(urlRequest: URLRequest(url: url)).response()
      harvestSongs(from: response.data)
      pending.send(Int32(response.urlResponse.statusCode), response.data)
    } catch {
      // MusicDataRequest throws for non-2xx responses, with the status inside.
      if let failure = error as? MusicDataRequest.Error {
        pending.send(Int32(failure.status), failure.detailText)
      } else {
        pending.send(-1, String(describing: error))
      }
    }
  }
}

/// Any method against api.music.apple.com, for changing the user's library.
@_cdecl("am_send")
public func amSend(
  _ method: UnsafePointer<CChar>, _ path: UnsafePointer<CChar>, _ body: UnsafePointer<CChar>,
  _ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply
) {
  let pending = Pending(context: context, reply: reply)
  let method = String(cString: method)
  let path = String(cString: path)
  let body = String(cString: body)
  Task {
    guard let url = URL(string: "https://api.music.apple.com" + path) else {
      pending.send(-1, "bad path: \(path)")
      return
    }
    var request = URLRequest(url: url)
    request.httpMethod = method
    if !body.isEmpty {
      request.httpBody = Data(body.utf8)
      request.setValue("application/json", forHTTPHeaderField: "Content-Type")
    }
    do {
      let response = try await MusicDataRequest(urlRequest: request).response()
      pending.send(Int32(response.urlResponse.statusCode), response.data)
    } catch let failure as MusicDataRequest.Error {
      pending.send(Int32(failure.status), failure.detailText)
    } catch {
      pending.send(-1, String(describing: error))
    }
  }
}

// MARK: - Playback

@MainActor private func describe(_ song: Song) -> [String: Any] {
  [
    "id": song.id.rawValue,
    "title": song.title,
    "artist": song.artistName,
    "album": song.albumTitle ?? "",
    "durationMs": Int((song.duration ?? 0) * 1000),
    "art": song.artwork?.url(width: 640, height: 640)?.absoluteString ?? "",
  ]
}

/// Adds a song to the end of what's queued.
@_cdecl("am_enqueue")
public func amEnqueue(_ id: UnsafePointer<CChar>, _ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  let id = String(cString: id)
  Task { @MainActor in
    do {
      guard let song = try await resolveSongs([id]).first else {
        pending.send(-1, "Apple Music didn't return this song")
        return
      }
      try await ApplicationMusicPlayer.shared.queue.insert(song, position: .tail)
      pending.send(200)
    } catch {
      pending.send(-1, String(describing: error))
    }
  }
}

/// The songs after the current one, as {"current":{...},"next":[{...}]}.
@_cdecl("am_queue")
public func amQueue(_ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  Task { @MainActor in
    let queue = ApplicationMusicPlayer.shared.queue
    var result: [String: Any] = [:]
    var next: [[String: Any]] = []
    var passed = queue.currentEntry == nil
    for entry in queue.entries {
      // Not every entry resolves to a song, but each has a title of its own.
      var row: [String: Any]
      if case let .song(song)? = entry.item {
        row = describe(song)
      } else {
        row = ["id": "entry-\(entry.id)", "title": entry.title, "artist": entry.subtitle ?? "", "album": "", "durationMs": 0, "art": ""]
      }
      if passed {
        next.append(row)
      } else if entry.id == queue.currentEntry?.id {
        result["current"] = row
        passed = true
      }
    }
    result["next"] = next
    pending.sendJSON(result)
  }
}

/// {"ids":[...], "start":0, "positionMs":0, "play":true}. Replies with timings.
@_cdecl("am_load")
public func amLoad(_ json: UnsafePointer<CChar>, _ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  let text = String(cString: json)
  Task { @MainActor in
    do {
      guard let spec = try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any],
        let ids = spec["ids"] as? [String], !ids.isEmpty
      else {
        pending.send(-1, "nothing to play")
        return
      }
      // A warm-up never replaces anything: a song may already be loaded.
      let warm = (spec["warm"] as? Bool) ?? false
      if warm, ApplicationMusicPlayer.shared.queue.currentEntry != nil {
        pending.sendJSON(["skipped": true])
        return
      }
      let began = Date()
      let start = min(max(spec["start"] as? Int ?? 0, 0), ids.count - 1)
      // One request can name 300 songs; a longer list starts around its chosen song.
      let first = max(0, min(start, ids.count - 300))
      let window = Array(ids[first..<min(first + 300, ids.count)])
      let libraryIDs = spec["libraryIDs"] as? [String: String] ?? [:]
      let songs = try await resolveSongs(window, libraryIDs: libraryIDs)
      // Songs are found under the ID asked for, which a fallback's own ID differs from.
      let present = window.filter { knownSongs[$0] != nil }
      guard !songs.isEmpty else {
        pending.send(-1, "Apple Music didn't return these songs")
        return
      }
      let startIndex = present.firstIndex(of: ids[start]) ?? 0
      let resolved = Date()
      let player = ApplicationMusicPlayer.shared
      if warm, player.queue.currentEntry != nil {
        pending.sendJSON(["skipped": true])
        return
      }
      let playing = (spec["play"] as? Bool) ?? true
      func begin() async throws {
        if playing { try await player.play() } else { try await player.prepareToPlay() }
      }
      if startIndex == 0 {
        player.queue = ApplicationMusicPlayer.Queue(for: songs)
        try await begin()
      } else {
        // `startingAt` can refuse a song with "unexpected start item". Starting
        // the list at that song still plays it, without the songs before it.
        do {
          player.queue = ApplicationMusicPlayer.Queue(for: songs, startingAt: songs[startIndex])
          try await begin()
        } catch {
          NSLog("am_load: startingAt failed, trimming the queue: %@", String(describing: error))
          player.queue = ApplicationMusicPlayer.Queue(for: Array(songs[startIndex...]))
          try await begin()
        }
      }
      if let position = spec["positionMs"] as? Int, position > 0 {
        player.playbackTime = TimeInterval(position) / 1000
      }
      let dropped = window.filter { knownSongs[$0] == nil }
      pending.sendJSON([
        "requested": ids.count,
        "queued": songs.count,
        "dropped": Array(dropped.prefix(12)),
        "resolveMs": Int(resolved.timeIntervalSince(began) * 1000),
        "playMs": Int(Date().timeIntervalSince(resolved) * 1000),
      ])
    } catch {
      pending.send(-1, String(describing: error))
    }
  }
}

/// Rearranges what comes after the current song to follow `ids`, leaving the song
/// itself playing. {"ids":[...], "libraryIDs":{...}}. Replies {"applied":bool}.
@_cdecl("am_requeue")
public func amRequeue(_ json: UnsafePointer<CChar>, _ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  let text = String(cString: json)
  Task { @MainActor in
    do {
      let player = ApplicationMusicPlayer.shared
      guard let spec = try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any],
        let ids = spec["ids"] as? [String],
        player.state.shuffleMode == .off,
        let current = player.queue.currentEntry,
        let position = player.queue.entries.firstIndex(where: { $0.id == current.id })
      else {
        pending.sendJSON(["applied": false])
        return
      }
      let window = Array(ids.prefix(300))
      let songs = try await resolveSongs(window, libraryIDs: spec["libraryIDs"] as? [String: String] ?? [:])
      // Songs after the current one in the new order; all of them if it was removed.
      var upcoming = songs
      if case let .song(playing)? = current.item,
        let at = songs.firstIndex(where: { $0.id == playing.id })
      {
        upcoming = Array(songs[songs.index(after: at)...])
      } else if let wanted = current.item?.id.rawValue, let at = songs.firstIndex(where: { $0.id.rawValue == wanted }) {
        upcoming = Array(songs[songs.index(after: at)...])
      }
      let entries = player.queue.entries
      let start = entries.index(after: position)
      player.queue.entries.replaceSubrange(start..<entries.endIndex, with: upcoming.map { ApplicationMusicPlayer.Queue.Entry($0) })
      pending.sendJSON(["applied": true, "upcoming": upcoming.count])
    } catch {
      pending.send(-1, String(describing: error))
    }
  }
}

/// 1 toggle, 2 next, 3 previous, 4 seek to `value` ms, 5 shuffle `value` != 0,
/// 6 repeat (0 off, 1 all, 2 one), 7 stop.
@_cdecl("am_control")
public func amControl(_ operation: Int32, _ value: Double) {
  Task { @MainActor in
    let player = ApplicationMusicPlayer.shared
    do {
      switch operation {
      case 1:
        if player.state.playbackStatus == .playing { player.pause() } else { try await player.play() }
      case 2: try await player.skipToNextEntry()
      case 3: try await player.skipToPreviousEntry()
      case 4: player.playbackTime = value / 1000
      case 5: player.state.shuffleMode = value != 0 ? .songs : .off
      case 6: player.state.repeatMode = value == 2 ? .one : value == 1 ? .all : MusicPlayer.RepeatMode.none
      case 7: player.stop()
      default: break
      }
    } catch {
      NSLog("am_control %d failed: %@", operation, String(describing: error))
    }
  }
}

/// Jumps `count` songs ahead at once, for a click on a row further down the queue.
@_cdecl("am_skip")
public func amSkip(_ count: Int32) {
  Task { @MainActor in
    let player = ApplicationMusicPlayer.shared
    do {
      if count > 1 {
        let entries = Array(player.queue.entries)
        if let current = player.queue.currentEntry,
          let index = entries.firstIndex(where: { $0.id == current.id }),
          index + Int(count) < entries.count
        {
          player.queue.currentEntry = entries[index + Int(count)]
          try await player.play()
          return
        }
      }
      try await player.skipToNextEntry()
      if player.state.playbackStatus != .playing { try await player.play() }
    } catch {
      NSLog("am_skip %d failed: %@", count, String(describing: error))
    }
  }
}

/// Stops the player and waits for it, for quitting: its process outlives ours.
@_cdecl("am_stop")
public func amStop(_ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  Task { @MainActor in
    ApplicationMusicPlayer.shared.stop()
    pending.send(200)
  }
}

@_cdecl("am_state")
public func amState(_ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  Task { @MainActor in
    let player = ApplicationMusicPlayer.shared
    var state: [String: Any] = [
      "status": String(describing: player.state.playbackStatus),
      "positionMs": Int(player.playbackTime * 1000),
      "shuffle": player.state.shuffleMode == .songs,
      "repeat": player.state.repeatMode == .one ? 2 : player.state.repeatMode == .all ? 1 : 0,
    ]
    if let entry = player.queue.currentEntry {
      state["entryTitle"] = entry.title
      state["entrySubtitle"] = entry.subtitle ?? ""
      // An upload's entry may not resolve to a song; it still has a title.
      state["id"] = "entry-\(entry.id)"
      state["title"] = entry.title
      state["artist"] = entry.subtitle ?? ""
      state["album"] = ""
      state["durationMs"] = 0
      state["art"] = ""
      if case let .song(song)? = entry.item {
        state["id"] = song.id.rawValue
        state["title"] = song.title
        state["artist"] = song.artistName
        state["album"] = song.albumTitle ?? ""
        state["durationMs"] = Int((song.duration ?? 0) * 1000)
        state["art"] = song.artwork?.url(width: 640, height: 640)?.absoluteString ?? ""
      }
    }
    pending.sendJSON(state)
  }
}

// MARK: - The audio tap

// Reads the player process's output so the visualizers can react. Nothing is
// stored or played back: samples only reach an in-memory analysis ring.

private func address(_ selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
  AudioObjectPropertyAddress(mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
}

private func playerProcess() -> AudioObjectID? {
  var list = address(kAudioHardwarePropertyProcessObjectList)
  var size: UInt32 = 0
  guard AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &list, 0, nil, &size) == noErr else { return nil }
  var ids = [AudioObjectID](repeating: 0, count: Int(size) / MemoryLayout<AudioObjectID>.size)
  guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &list, 0, nil, &size, &ids) == noErr else { return nil }
  for id in ids {
    var bundle: Unmanaged<CFString>?
    var length = UInt32(MemoryLayout<CFString?>.size)
    var property = address(kAudioProcessPropertyBundleID)
    guard AudioObjectGetPropertyData(id, &property, 0, nil, &length, &bundle) == noErr,
      let name = bundle?.takeRetainedValue() as String?, name == "com.apple.MediaPlayer.RemotePlayerService"
    else { continue }
    return id
  }
  return nil
}

private final class PlayerTap: @unchecked Sendable {
  private let lock = NSLock()
  private var muted = false
  private var description: CATapDescription?
  private var tap = AudioObjectID(kAudioObjectUnknown)
  private var device = AudioObjectID(kAudioObjectUnknown)
  private var procID: AudioDeviceIOProcID?
  private var source = AudioObjectID(kAudioObjectUnknown)
  private var outputDevice = AudioDeviceID(0)
  private let queue = DispatchQueue(label: "am.tap", qos: .userInteractive)
  private let context: UnsafeMutableRawPointer?
  private let callback: AMAudio

  init(context: UnsafeMutableRawPointer?, callback: @escaping AMAudio) {
    self.context = context
    self.callback = callback
  }

  private func defaultOutput() -> (id: AudioDeviceID, uid: String) {
    var device = AudioDeviceID(0)
    var size = UInt32(MemoryLayout<AudioDeviceID>.size)
    var property = address(kAudioHardwarePropertyDefaultOutputDevice)
    AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &property, 0, nil, &size, &device)
    var uid: Unmanaged<CFString>?
    size = UInt32(MemoryLayout<CFString?>.size)
    property = address(kAudioDevicePropertyDeviceUID)
    AudioObjectGetPropertyData(device, &property, 0, nil, &size, &uid)
    return (device, uid?.takeRetainedValue() as String? ?? "")
  }

  /// Follows the player process and the default output as they come and go.
  func refresh() {
    lock.lock()
    defer { lock.unlock() }
    let process = playerProcess()
    let output = defaultOutput()
    if process == source, output.id == outputDevice { return }
    close()
    guard let process else { return }
    open(process: process, output: output)
  }

  private func open(process: AudioObjectID, output: (id: AudioDeviceID, uid: String)) {
    let description = CATapDescription(stereoMixdownOfProcesses: [process])
    description.uuid = UUID()
    description.isPrivate = true
    description.muteBehavior = muted ? .mutedWhenTapped : .unmuted
    self.description = description
    guard AudioHardwareCreateProcessTap(description, &tap) == noErr else {
      NSLog("am tap: could not create")
      return
    }
    var format = AudioStreamBasicDescription()
    var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
    var property = address(kAudioTapPropertyFormat)
    AudioObjectGetPropertyData(tap, &property, 0, nil, &size, &format)
    let rate = UInt32(format.mSampleRate)

    let aggregate: [String: Any] = [
      kAudioAggregateDeviceNameKey: "Apple Music client tap",
      kAudioAggregateDeviceUIDKey: UUID().uuidString,
      kAudioAggregateDeviceMainSubDeviceKey: output.uid,
      kAudioAggregateDeviceIsPrivateKey: true,
      kAudioAggregateDeviceIsStackedKey: false,
      kAudioAggregateDeviceTapAutoStartKey: true,
      kAudioAggregateDeviceSubDeviceListKey: [[kAudioSubDeviceUIDKey: output.uid]],
      kAudioAggregateDeviceTapListKey: [
        [kAudioSubTapDriftCompensationKey: true, kAudioSubTapUIDKey: description.uuid.uuidString]
      ],
    ]
    guard AudioHardwareCreateAggregateDevice(aggregate as CFDictionary, &device) == noErr else {
      NSLog("am tap: could not create the aggregate device")
      close()
      return
    }
    let context = self.context
    let callback = self.callback
    let status = AudioDeviceCreateIOProcIDWithBlock(&procID, device, queue) { _, input, _, _, _ in
      for buffer in UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: input)) {
        guard let data = buffer.mData else { continue }
        callback(context, data.assumingMemoryBound(to: Float.self), Int(buffer.mDataByteSize) / 4, rate)
      }
    }
    guard status == noErr, let procID, AudioDeviceStart(device, procID) == noErr else {
      NSLog("am tap: could not start")
      close()
      return
    }
    source = process
    outputDevice = output.id
  }

  fileprivate var tapIsOpen: Bool { tap != kAudioObjectUnknown }

  fileprivate func updateTap(_ description: CATapDescription) {
    var property = address(kAudioTapPropertyDescription)
    var value = description
    let status = withUnsafePointer(to: &value) {
      AudioObjectSetPropertyData(tap, &property, 0, nil, UInt32(MemoryLayout<CATapDescription>.size), $0)
    }
    if status != noErr { NSLog("am tap: could not change muting: %d", status) }
  }

  private func close() {
    if let procID {
      AudioDeviceStop(device, procID)
      AudioDeviceDestroyIOProcID(device, procID)
    }
    procID = nil
    if device != kAudioObjectUnknown { AudioHardwareDestroyAggregateDevice(device) }
    device = AudioObjectID(kAudioObjectUnknown)
    if tap != kAudioObjectUnknown { AudioHardwareDestroyProcessTap(tap) }
    tap = AudioObjectID(kAudioObjectUnknown)
    source = AudioObjectID(kAudioObjectUnknown)
    outputDevice = AudioDeviceID(0)
  }
}

/// Silences the player on this Mac while its audio goes to a speaker, or brings it back.
private nonisolated(unsafe) var sharedTap: PlayerTap?

extension PlayerTap {
  func setMuted(_ wanted: Bool) {
    lock.lock()
    defer { lock.unlock() }
    muted = wanted
    guard tapIsOpen, let description else { return }
    description.muteBehavior = wanted ? .mutedWhenTapped : .unmuted
    updateTap(description)
  }
}

@_cdecl("am_tap_mute")
public func amTapMute(_ muted: Bool) {
  sharedTap?.setMuted(muted)
}

/// Starts following the player's audio for the life of the app. Rust keeps `context` alive.
@_cdecl("am_tap_start")
public func amTapStart(_ context: UnsafeMutableRawPointer?, _ callback: @escaping AMAudio) {
  let tap = PlayerTap(context: context, callback: callback)
  sharedTap = tap
  Task.detached(priority: .utility) {
    while true {
      tap.refresh()
      try? await Task.sleep(for: .seconds(1))
    }
  }
}

// MARK: - Speakers

/// HomePods on the network. Other AirPlay receivers ask for a PIN or are this Mac.
private final class Speakers: @unchecked Sendable {
  static let shared = Speakers()
  private let lock = NSLock()
  private var found: [String: NWEndpoint] = [:]
  private let queue = DispatchQueue(label: "am.speakers")
  private var browser: NWBrowser?

  func start() {
    let browser = NWBrowser(for: .bonjourWithTXTRecord(type: "_airplay._tcp", domain: nil), using: .tcp)
    browser.browseResultsChangedHandler = { [weak self] results, _ in
      var speakers: [String: NWEndpoint] = [:]
      for result in results {
        guard case let .service(name, _, _, _) = result.endpoint,
          case let .bonjour(record) = result.metadata,
          record["model"]?.hasPrefix("AudioAccessory") == true
        else { continue }
        speakers[name] = result.endpoint
      }
      NSLog("am speakers: %d results, %d HomePods", results.count, speakers.count)
      self?.lock.lock()
      self?.found = speakers
      self?.lock.unlock()
    }
    browser.stateUpdateHandler = { state in
      NSLog("am speakers: browser state %@", String(describing: state))
    }
    browser.start(queue: queue)
    self.browser = browser
  }

  var names: [String] {
    lock.lock()
    defer { lock.unlock() }
    return found.keys.sorted()
  }

  func endpoint(_ name: String) -> NWEndpoint? {
    lock.lock()
    defer { lock.unlock() }
    return found[name]
  }
}

@_cdecl("am_speakers_start")
public func amSpeakersStart() {
  Speakers.shared.start()
}

@_cdecl("am_speakers")
public func amSpeakers(_ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  pending.sendJSON(["names": Speakers.shared.names])
}

/// Connects once to find the speaker's IPv4 address: {"host":"...","port":7000}.
@_cdecl("am_speaker_resolve")
public func amSpeakerResolve(_ name: UnsafePointer<CChar>, _ context: UnsafeMutableRawPointer?, _ reply: @escaping AMReply) {
  let pending = Pending(context: context, reply: reply)
  guard let endpoint = Speakers.shared.endpoint(String(cString: name)) else {
    pending.send(-1, "That speaker isn't on the network right now")
    return
  }
  let parameters = NWParameters.tcp
  if let ip = parameters.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options { ip.version = .v4 }
  let connection = NWConnection(to: endpoint, using: parameters)
  let finished = OSAllocatedUnfairLockBox()
  connection.stateUpdateHandler = { state in
    switch state {
    case .ready:
      guard finished.claim() else { return }
      if case let .hostPort(host, port)? = connection.currentPath?.remoteEndpoint {
        let address = "\(host)".split(separator: "%").first.map(String.init) ?? "\(host)"
        pending.sendJSON(["host": address, "port": Int(port.rawValue)])
      } else {
        pending.send(-1, "The speaker didn't report an address")
      }
      connection.cancel()
    case let .failed(error):
      guard finished.claim() else { return }
      pending.send(-1, String(describing: error))
      connection.cancel()
    default:
      break
    }
  }
  connection.start(queue: .global())
  DispatchQueue.global().asyncAfter(deadline: .now() + 8) {
    guard finished.claim() else { return }
    pending.send(-1, "Timed out reaching the speaker")
    connection.cancel()
  }
}

/// Lets exactly one of several callbacks answer.
private final class OSAllocatedUnfairLockBox: @unchecked Sendable {
  private let lock = NSLock()
  private var done = false
  func claim() -> Bool {
    lock.lock()
    defer { lock.unlock() }
    if done { return false }
    done = true
    return true
  }
}

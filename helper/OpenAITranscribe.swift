import Foundation

/// `openai-stream`: transcribes a recording with OpenAI's GPT Live Transcribe
/// (`gpt-live-transcribe`) over the Realtime API's transcription WebSocket, while it's made or
/// after. Reads 16 kHz mono 16-bit little-endian PCM on stdin, resamples it to the API's 24 kHz,
/// and writes JSON lines: `{"ready":true}` once the session is accepted, one
/// `{"segment":{t0,t1,text}}` per committed stretch of speech (times are ms from the first
/// sample; the words inside a segment aren't timed), and after stdin closes and the last stretch
/// is transcribed `{"done":true,"samples":N}`. The key comes from `SCRAWL_OPENAI_API_KEY`, never
/// an argument. Any failure goes to stderr as a fixed sentence (never the provider's message,
/// which can quote the key) and exits 1, and Scrawl transcribes on this Mac instead.
///
/// The model has no server-side turn detection, so the helper commits the buffer itself: after
/// a stretch with speech ends in `commitSilenceMs` of quiet, or when `maxStretchMs` of audio is
/// waiting. Each commit's audio range is remembered in order, so the `committed` acknowledgement
/// gives the item its time range and `completed` its text.
enum OpenAITranscriber {
  static let model = "gpt-live-transcribe"
  static let endpoint = URL(string: "wss://api.openai.com/v1/realtime?intent=transcription")!

  static func run(language: String?) async throws {
    guard let key = ProcessInfo.processInfo.environment["SCRAWL_OPENAI_API_KEY"], !key.isEmpty else {
      throw HelperError("no API key in SCRAWL_OPENAI_API_KEY")
    }
    let session = OpenAISession(key: key, language: language ?? "en")
    try await session.run()
  }

  /// What a provider error means for the user, without quoting it.
  static func describe(code: String?, status: Int?) -> String {
    if status == 401 || code == "invalid_api_key" || code == "invalid_authentication" {
      return "OpenAI didn't accept the API key"
    }
    if status == 403 || code == "model_not_found" || code == "permission_denied" {
      return "this API key can't use \(model)"
    }
    if status == 429 || code == "rate_limit_exceeded" || code == "insufficient_quota" {
      return "OpenAI's usage limit was reached"
    }
    return "OpenAI couldn't complete the transcription"
  }
}

private struct Item {
  var range: (t0: Int, t1: Int)
  var text = ""
  var done = false
}

private actor OpenAISession {
  static let inputRate = 16_000
  static let outputRate = 24_000
  /// Quiet this long after speech ends a stretch.
  static let commitSilenceMs = 600
  /// A stretch is never longer than this; one is committed in the middle of speech if need be.
  static let maxStretchMs = 30_000
  /// The API wants at least 100 ms per commit; a shorter tail is padded with silence.
  static let minCommitMs = 100
  static let windowMs = 50
  static let speechRms = 0.008
  static let connectTimeout: UInt64 = 15
  static let finishTimeout: UInt64 = 25

  let key: String
  let language: String
  var task: URLSessionWebSocketTask?
  var ready = false
  var failed = false
  var queued: [Data] = []
  /// Input samples read so far.
  var total = 0
  /// Where the audio not yet committed starts, in input samples.
  var stretchStart = 0
  var stretchSpeech = false
  var trailingSilenceMs = 0
  var everCommitted = false
  /// Audio ranges (ms) of commits the server hasn't acknowledged yet, oldest first.
  var pendingCommits: [(Int, Int)] = []
  var items: [String: Item] = [:]
  var inputClosed = false
  var finished: CheckedContinuation<Void, Error>?
  var watchdog: Task<Void, Never>?
  // Resampling state: the last input sample, for interpolation across chunks.
  var lastSample: Int16 = 0
  /// Output samples made so far.
  var produced = 0

  init(key: String, language: String) {
    self.key = key
    self.language = language
  }

  func run() async throws {
    var request = URLRequest(url: OpenAITranscriber.endpoint)
    request.setValue("Bearer \(key)", forHTTPHeaderField: "Authorization")
    let task = URLSession.shared.webSocketTask(with: request)
    self.task = task
    task.maximumMessageSize = 4 << 20
    task.resume()
    armWatchdog(seconds: Self.connectTimeout, reason: "OpenAI didn't answer in time")
    send([
      "type": "session.update",
      "session": [
        "type": "transcription",
        "audio": [
          "input": [
            "format": ["type": "audio/pcm", "rate": Self.outputRate],
            "transcription": ["model": OpenAITranscriber.model, "languages": [language], "delay": "medium"],
            "turn_detection": NSNull(),
          ],
        ],
      ],
    ])
    Task { await self.receiveLoop(task) }
    Task.detached { await self.pumpStdin() }
    try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
      self.finished = continuation
    }
  }

  // MARK: Audio in

  private nonisolated func pumpStdin() async {
    let stdin = FileHandle.standardInput
    var pending = Data()
    while true {
      let chunk = stdin.availableData
      if chunk.isEmpty { break }
      pending.append(chunk)
      let usable = pending.count & ~1
      guard usable >= Self.inputRate * Self.windowMs / 1000 * 2 else { continue }
      let bytes = pending.prefix(usable)
      pending.removeFirst(usable)
      await feed(bytes)
    }
    let usable = pending.count & ~1
    if usable > 0 { await feed(pending.prefix(usable)) }
    await inputEnded()
  }

  private func feed(_ bytes: Data) {
    guard !failed else { return }
    let count = bytes.count / 2
    var samples = [Int16](repeating: 0, count: count)
    bytes.withUnsafeBytes { raw in
      let source = raw.bindMemory(to: Int16.self)
      for i in 0..<count { samples[i] = Int16(littleEndian: source[i]) }
    }
    append(resample(samples))
    // Pause detection, window by window.
    let window = Self.inputRate * Self.windowMs / 1000
    var index = 0
    while index < count {
      let end = min(index + window, count)
      var energy = 0.0
      for i in index..<end {
        let s = Double(samples[i]) / 32_768
        energy += s * s
      }
      let rms = (energy / Double(end - index)).squareRoot()
      let ms = (end - index) * 1000 / Self.inputRate
      if rms > Self.speechRms {
        stretchSpeech = true
        trailingSilenceMs = 0
      } else {
        trailingSilenceMs += ms
      }
      index = end
    }
    total += count
    let stretchMs = (total - stretchStart) * 1000 / Self.inputRate
    if stretchSpeech && trailingSilenceMs >= Self.commitSilenceMs && stretchMs - trailingSilenceMs >= 300 {
      commitStretch()
    } else if stretchMs >= Self.maxStretchMs && stretchSpeech {
      commitStretch()
    } else if stretchMs >= Self.maxStretchMs {
      // A long silence: drop it from the next stretch without asking the model about it.
      stretchStart = total
      trailingSilenceMs = 0
    }
  }

  /// Linear interpolation from 16 kHz to 24 kHz (2:3), continuous across chunks.
  private func resample(_ input: [Int16]) -> Data {
    var out = Data(capacity: input.count * 3)
    var prev = Double(lastSample)
    // Output j sits at input position 2j/3; those in (i-1, i] interpolate prev→cur.
    for (k, cur) in input.enumerated() {
      let i = total + k
      let curD = Double(cur)
      while produced * 2 <= i * 3 {
        let frac = Double(produced) * 2 / 3 - Double(i - 1)
        let value = prev + (curD - prev) * frac
        var sample = Int16(max(-32_768, min(32_767, value.rounded())))
        withUnsafeBytes(of: &sample) { out.append(contentsOf: $0) }
        produced += 1
      }
      prev = curD
    }
    if let last = input.last { lastSample = last }
    return out
  }

  private func append(_ pcm24k: Data) {
    guard !pcm24k.isEmpty else { return }
    if ready {
      send(["type": "input_audio_buffer.append", "audio": pcm24k.base64EncodedString()])
    } else {
      queued.append(pcm24k)
      if queued.reduce(0, { $0 + $1.count }) > 8 << 20 {
        fail("OpenAI took too long to accept the session")
      }
    }
  }

  private func commitStretch() {
    let t0 = stretchStart * 1000 / Self.inputRate
    let t1 = total * 1000 / Self.inputRate
    pendingCommits.append((t0, t1))
    if ready { send(["type": "input_audio_buffer.commit"]) }
    everCommitted = true
    stretchStart = total
    stretchSpeech = false
    trailingSilenceMs = 0
  }

  private func inputEnded() {
    inputClosed = true
    guard !failed else { return }
    let waiting = total - stretchStart
    if waiting > 0 && (stretchSpeech || !everCommitted) {
      let minimum = Self.inputRate * Self.minCommitMs / 1000
      if waiting < minimum {
        let silence = Data(count: (minimum - waiting) * 3)  // 24 kHz: 3 bytes per 16 kHz sample
        append(silence)
      }
      commitStretch()
    }
    armWatchdog(seconds: Self.finishTimeout, reason: "OpenAI didn't finish in time")
    maybeDone()
  }

  // MARK: Events

  private func receiveLoop(_ task: URLSessionWebSocketTask) async {
    while !failed, finished != nil {
      do {
        let message = try await task.receive()
        switch message {
        case .string(let text): handle(Data(text.utf8))
        case .data(let data): handle(data)
        @unknown default: break
        }
      } catch {
        if finished != nil {
          let status = (task.response as? HTTPURLResponse)?.statusCode
          fail(OpenAITranscriber.describe(code: nil, status: status))
        }
        return
      }
    }
  }

  private func handle(_ data: Data) {
    guard let event = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
      let type = event["type"] as? String
    else { return }
    switch type {
    case "session.created":
      break
    case "session.updated":
      guard !ready else { return }
      ready = true
      watchdog?.cancel()
      emit(["ready": true])
      for audio in queued {
        send(["type": "input_audio_buffer.append", "audio": audio.base64EncodedString()])
      }
      queued.removeAll()
      for _ in pendingCommits { send(["type": "input_audio_buffer.commit"]) }
      if inputClosed { armWatchdog(seconds: Self.finishTimeout, reason: "OpenAI didn't finish in time") }
      maybeDone()
    case "input_audio_buffer.committed":
      guard let id = event["item_id"] as? String, !pendingCommits.isEmpty else { return }
      let range = pendingCommits.removeFirst()
      var item = items[id] ?? Item(range: (range.0, range.1))
      item.range = (range.0, range.1)
      items[id] = item
      if item.done { emitSegment(item) }
      maybeDone()
    case "conversation.item.input_audio_transcription.delta":
      guard let id = event["item_id"] as? String, let delta = event["delta"] as? String else { return }
      var item = items[id] ?? Item(range: (-1, -1))
      if !item.done { item.text += delta }
      items[id] = item
    case "conversation.item.input_audio_transcription.completed":
      guard let id = event["item_id"] as? String else { return }
      var item = items[id] ?? Item(range: (-1, -1))
      item.text = event["transcript"] as? String ?? item.text
      item.done = true
      items[id] = item
      if item.range.t0 >= 0 { emitSegment(item) }
      maybeDone()
    case "error", "conversation.item.input_audio_transcription.failed":
      let error = event["error"] as? [String: Any]
      fail(OpenAITranscriber.describe(code: error?["code"] as? String, status: nil))
    default:
      break
    }
  }

  private func emitSegment(_ item: Item) {
    let text = item.text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    guard !text.isEmpty else { return }
    emit(["segment": ["t0": item.range.t0, "t1": item.range.t1, "text": text]])
  }

  private func maybeDone() {
    guard inputClosed, pendingCommits.isEmpty, items.values.allSatisfy(\.done) else { return }
    watchdog?.cancel()
    emit(["done": true, "samples": total])
    task?.cancel(with: .normalClosure, reason: nil)
    finished?.resume()
    finished = nil
  }

  private func fail(_ reason: String) {
    guard !failed else { return }
    failed = true
    watchdog?.cancel()
    task?.cancel(with: .normalClosure, reason: nil)
    finished?.resume(throwing: HelperError(reason))
    finished = nil
  }

  private func armWatchdog(seconds: UInt64, reason: String) {
    watchdog?.cancel()
    watchdog = Task { [weak self] in
      try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
      guard !Task.isCancelled else { return }
      await self?.fail(reason)
    }
  }

  private func send(_ object: [String: Any]) {
    guard let task, let data = try? JSONSerialization.data(withJSONObject: object),
      let text = String(data: data, encoding: .utf8)
    else { return }
    task.send(.string(text)) { [weak self] error in
      guard error != nil, let self else { return }
      Task { await self.fail("the connection to OpenAI dropped") }
    }
  }
}

import Foundation
import WebRTC

enum ProbeError: Error, CustomStringConvertible {
    case invalidResponse(String)
    case rpc(String)
    case timeout(String)
    case webSocket(String)
    case webRTC(String)

    var description: String {
        switch self {
        case .invalidResponse(let detail):
            return "invalid response: \(detail)"
        case .rpc(let detail):
            return "RPC failed: \(detail)"
        case .timeout(let operation):
            return "timed out waiting for \(operation)"
        case .webSocket(let detail):
            return "WebSocket failed: \(detail)"
        case .webRTC(let detail):
            return "WebRTC failed: \(detail)"
        }
    }
}

struct RtpCounters {
    let inboundPackets: UInt64
    let inboundBytes: UInt64
    let outboundPackets: UInt64
    let outboundBytes: UInt64
    let inboundSsrcs: [UInt64]
    let outboundSsrcs: [UInt64]
}

final class RpcClient {
    private let session: URLSession
    private let task: URLSessionWebSocketTask
    private var latestStats: [String: Any]?

    init(url: URL) {
        session = URLSession(configuration: .ephemeral)
        task = session.webSocketTask(with: url)
    }

    func connect() {
        task.resume()
    }

    func close() {
        task.cancel(with: .normalClosure, reason: nil)
        session.invalidateAndCancel()
    }

    func call(id: String, method: String, params: [String: Any]) throws -> [String: Any] {
        let request: [String: Any] = [
            "id": id,
            "method": method,
            "params": params,
        ]
        let requestData = try JSONSerialization.data(withJSONObject: request)
        guard let requestText = String(data: requestData, encoding: .utf8) else {
            throw ProbeError.invalidResponse("could not encode request")
        }

        let sendSemaphore = DispatchSemaphore(value: 0)
        var sendError: Error?
        task.send(.string(requestText)) { error in
            sendError = error
            sendSemaphore.signal()
        }
        guard sendSemaphore.wait(timeout: .now() + 10) == .success else {
            throw ProbeError.timeout("WebSocket send for \(method)")
        }
        if let sendError {
            throw ProbeError.webSocket(sendError.localizedDescription)
        }

        while true {
            let receiveSemaphore = DispatchSemaphore(value: 0)
            var receivedMessage: URLSessionWebSocketTask.Message?
            var receiveError: Error?
            task.receive { result in
                switch result {
                case .success(let message):
                    receivedMessage = message
                case .failure(let error):
                    receiveError = error
                }
                receiveSemaphore.signal()
            }
            guard receiveSemaphore.wait(timeout: .now() + 15) == .success else {
                throw ProbeError.timeout("WebSocket response for \(method)")
            }
            if let receiveError {
                throw ProbeError.webSocket(receiveError.localizedDescription)
            }
            guard let receivedMessage else {
                throw ProbeError.invalidResponse("empty WebSocket response")
            }

            let responseData: Data
            switch receivedMessage {
            case .string(let text):
                responseData = Data(text.utf8)
            case .data(let data):
                responseData = data
            @unknown default:
                throw ProbeError.invalidResponse("unknown WebSocket message type")
            }

            guard let response = try JSONSerialization.jsonObject(with: responseData) as? [String: Any]
            else {
                throw ProbeError.invalidResponse("response is not a JSON object")
            }

            // State-change events can arrive between request and response.
            guard response["id"] as? String == id else {
                if let event = response["event"] as? String {
                    print("rtpbridge event: \(event)")
                    if event == "stats", let data = response["data"] as? [String: Any] {
                        latestStats = data
                    }
                }
                continue
            }
            if let error = response["error"] {
                throw ProbeError.rpc(String(describing: error))
            }
            guard let result = response["result"] as? [String: Any] else {
                throw ProbeError.invalidResponse("missing result for \(method)")
            }
            return result
        }
    }

    func endpointCounters(endpointID: String) -> RtpCounters? {
        guard
            let endpoints = latestStats?["endpoints"] as? [[String: Any]],
            let endpoint = endpoints.first(where: { $0["endpoint_id"] as? String == endpointID }),
            let inbound = endpoint["inbound"] as? [String: Any],
            let outbound = endpoint["outbound"] as? [String: Any]
        else {
            return nil
        }

        return RtpCounters(
            inboundPackets: (inbound["packets"] as? NSNumber)?.uint64Value ?? 0,
            inboundBytes: (inbound["bytes"] as? NSNumber)?.uint64Value ?? 0,
            outboundPackets: (outbound["packets"] as? NSNumber)?.uint64Value ?? 0,
            outboundBytes: (outbound["bytes"] as? NSNumber)?.uint64Value ?? 0,
            inboundSsrcs: [],
            outboundSsrcs: []
        )
    }
}

final class PeerDelegate: NSObject, RTCPeerConnectionDelegate {
    let gatheringComplete = DispatchSemaphore(value: 0)
    let firstCandidate = DispatchSemaphore(value: 0)
    let iceConnected = DispatchSemaphore(value: 0)
    let replacementPairSelected = DispatchSemaphore(value: 0)
    private(set) var selectedPairChanges = 0

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didChange stateChanged: RTCSignalingState
    ) {
        print("WebRTC signaling state: \(stateChanged.rawValue)")
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didAdd stream: RTCMediaStream) {
        print("WebRTC added remote stream")
    }

    func peerConnection(_ peerConnection: RTCPeerConnection, didRemove stream: RTCMediaStream) {
        print("WebRTC removed remote stream")
    }

    func peerConnectionShouldNegotiate(_ peerConnection: RTCPeerConnection) {
        print("WebRTC requested negotiation")
    }

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didChange newState: RTCIceConnectionState
    ) {
        print("WebRTC ICE connection state: \(newState.rawValue)")
        if newState == .connected || newState == .completed {
            iceConnected.signal()
        }
    }

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didChange newState: RTCIceGatheringState
    ) {
        print("WebRTC ICE gathering state: \(newState.rawValue)")
        if newState == .complete {
            gatheringComplete.signal()
        }
    }

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didGenerate candidate: RTCIceCandidate
    ) {
        print("WebRTC candidate: \(candidate.sdp)")
        firstCandidate.signal()
    }

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didRemove candidates: [RTCIceCandidate]
    ) {
        print("WebRTC removed \(candidates.count) candidate(s)")
    }

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didOpen dataChannel: RTCDataChannel
    ) {
        print("WebRTC opened data channel: \(dataChannel.label)")
    }

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didChangeLocalCandidate local: RTCIceCandidate,
        remoteCandidate remote: RTCIceCandidate,
        lastReceivedMs: Int32,
        changeReason reason: String
    ) {
        selectedPairChanges += 1
        print("WebRTC selected pair #\(selectedPairChanges): \(local.sdp) -> \(remote.sdp) (\(reason))")
        if selectedPairChanges >= 2 {
            replacementPairSelected.signal()
        }
    }

    func peerConnection(
        _ peerConnection: RTCPeerConnection,
        didChange stateChanged: RTCPeerConnectionState
    ) {
        print("WebRTC peer connection state: \(stateChanged.rawValue)")
    }
}

func setRemoteDescription(_ description: RTCSessionDescription, on peer: RTCPeerConnection) throws {
    let semaphore = DispatchSemaphore(value: 0)
    var operationError: Error?
    peer.setRemoteDescription(description) { error in
        operationError = error
        semaphore.signal()
    }
    guard semaphore.wait(timeout: .now() + 15) == .success else {
        throw ProbeError.timeout("setRemoteDescription")
    }
    if let operationError {
        throw ProbeError.webRTC("setRemoteDescription: \(operationError.localizedDescription)")
    }
}

func createAnswer(on peer: RTCPeerConnection, constraints: RTCMediaConstraints) throws -> RTCSessionDescription {
    let semaphore = DispatchSemaphore(value: 0)
    var answer: RTCSessionDescription?
    var operationError: Error?
    peer.answer(for: constraints) { result, error in
        answer = result
        operationError = error
        semaphore.signal()
    }
    guard semaphore.wait(timeout: .now() + 15) == .success else {
        throw ProbeError.timeout("createAnswer")
    }
    if let operationError {
        throw ProbeError.webRTC("createAnswer: \(operationError.localizedDescription)")
    }
    guard let answer else {
        throw ProbeError.webRTC("createAnswer returned neither SDP nor error")
    }
    return answer
}

func setLocalDescription(_ description: RTCSessionDescription, on peer: RTCPeerConnection) throws {
    let semaphore = DispatchSemaphore(value: 0)
    var operationError: Error?
    peer.setLocalDescription(description) { error in
        operationError = error
        semaphore.signal()
    }
    guard semaphore.wait(timeout: .now() + 15) == .success else {
        throw ProbeError.timeout("setLocalDescription")
    }
    if let operationError {
        throw ProbeError.webRTC("setLocalDescription: \(operationError.localizedDescription)")
    }
}

func iceOptions(in sdp: String) -> [String] {
    sdp.components(separatedBy: .newlines)
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        .filter { $0.hasPrefix("a=ice-options:") }
}

func remoteAddress(in sessionInfo: [String: Any], endpointID: String) -> String? {
    guard let endpoints = sessionInfo["endpoints"] as? [[String: Any]] else {
        return nil
    }
    return endpoints.first(where: { $0["endpoint_id"] as? String == endpointID })?["remote_rtp_addr"] as? String
}

func offerGeneration(in sessionInfo: [String: Any], endpointID: String) -> UInt64? {
    guard let endpoints = sessionInfo["endpoints"] as? [[String: Any]] else {
        return nil
    }
    return (endpoints.first(where: { $0["endpoint_id"] as? String == endpointID })?["offer_generation"] as? NSNumber)?.uint64Value
}

func rtpCounters(on peer: RTCPeerConnection) throws -> RtpCounters {
    let semaphore = DispatchSemaphore(value: 0)
    var counters = RtpCounters(
        inboundPackets: 0,
        inboundBytes: 0,
        outboundPackets: 0,
        outboundBytes: 0,
        inboundSsrcs: [],
        outboundSsrcs: []
    )

    peer.statistics { report in
        var inboundPackets: UInt64 = 0
        var inboundBytes: UInt64 = 0
        var outboundPackets: UInt64 = 0
        var outboundBytes: UInt64 = 0
        var inboundSsrcs = Set<UInt64>()
        var outboundSsrcs = Set<UInt64>()

        for statistic in report.statistics.values {
            let kind = statistic.values["kind"] as? String
            let mediaType = statistic.values["mediaType"] as? String
            guard kind == "audio" || mediaType == "audio" else {
                continue
            }

            switch statistic.type {
            case "inbound-rtp":
                inboundPackets += (statistic.values["packetsReceived"] as? NSNumber)?.uint64Value ?? 0
                inboundBytes += (statistic.values["bytesReceived"] as? NSNumber)?.uint64Value ?? 0
                if let ssrc = (statistic.values["ssrc"] as? NSNumber)?.uint64Value {
                    inboundSsrcs.insert(ssrc)
                }
            case "outbound-rtp":
                outboundPackets += (statistic.values["packetsSent"] as? NSNumber)?.uint64Value ?? 0
                outboundBytes += (statistic.values["bytesSent"] as? NSNumber)?.uint64Value ?? 0
                if let ssrc = (statistic.values["ssrc"] as? NSNumber)?.uint64Value {
                    outboundSsrcs.insert(ssrc)
                }
            default:
                break
            }
        }

        counters = RtpCounters(
            inboundPackets: inboundPackets,
            inboundBytes: inboundBytes,
            outboundPackets: outboundPackets,
            outboundBytes: outboundBytes,
            inboundSsrcs: inboundSsrcs.sorted(),
            outboundSsrcs: outboundSsrcs.sorted()
        )
        semaphore.signal()
    }

    guard semaphore.wait(timeout: .now() + 10) == .success else {
        throw ProbeError.timeout("WebRTC statistics")
    }
    return counters
}

func runProbe() throws {
    let urlText = CommandLine.arguments.dropFirst().first ?? "ws://127.0.0.1:19100"
    guard let url = URL(string: urlText) else {
        throw ProbeError.invalidResponse("invalid WebSocket URL: \(urlText)")
    }
    let expectReplacement = ProcessInfo.processInfo.environment["M144_EXPECT_RENOMINATION"] == "1"
    let sendAudio = ProcessInfo.processInfo.environment["M144_SEND_AUDIO"] == "1"
    let backupPingInterval = ProcessInfo.processInfo.environment["M144_BACKUP_PING_MS"]
        .flatMap(Int32.init)
    let receivingTimeout = ProcessInfo.processInfo.environment["M144_RECEIVING_TIMEOUT_MS"]
        .flatMap(Int32.init)
    let strongCheckInterval = ProcessInfo.processInfo.environment["M144_STRONG_CHECK_MS"]
        .flatMap(Int.init)
    let weakCheckInterval = ProcessInfo.processInfo.environment["M144_WEAK_CHECK_MS"]
        .flatMap(Int.init)
    let minimumCheckInterval = ProcessInfo.processInfo.environment["M144_MIN_CHECK_MS"]
        .flatMap(Int.init)
    let unwritableTimeout = ProcessInfo.processInfo.environment["M144_UNWRITABLE_TIMEOUT_MS"]
        .flatMap(Int.init)
    let unwritableMinChecks = ProcessInfo.processInfo.environment["M144_UNWRITABLE_MIN_CHECKS"]
        .flatMap(Int.init)
    let inactiveTimeout = ProcessInfo.processInfo.environment["M144_INACTIVE_TIMEOUT_MS"]
        .flatMap(Int.init)

    guard RTCInitializeSSL() else {
        throw ProbeError.webRTC("RTCInitializeSSL returned false")
    }
    defer { RTCCleanupSSL() }

    let rpc = RpcClient(url: url)
    rpc.connect()
    defer { rpc.close() }

    let session = try rpc.call(id: "1", method: "session.create", params: [:])
    guard let sessionID = session["session_id"] as? String else {
        throw ProbeError.invalidResponse("session.create omitted session_id")
    }
    print("rtpbridge session: \(sessionID)")

    let offerResult = try rpc.call(
        id: "2",
        method: "endpoint.webrtc.create_offer",
        params: ["direction": "sendrecv"]
    )
    guard
        let endpointID = offerResult["endpoint_id"] as? String,
        let offerSDP = offerResult["sdp_offer"] as? String
    else {
        throw ProbeError.invalidResponse("create_offer omitted endpoint_id or sdp_offer")
    }

    let offerOptions = iceOptions(in: offerSDP)
    print("rtpbridge offer ICE options: \(offerOptions)")
    guard offerOptions.contains(where: { $0.split(separator: ":", maxSplits: 1).last?.split(separator: " ").contains("renomination") == true }) else {
        throw ProbeError.webRTC("rtpbridge offer did not advertise renomination")
    }

    let factory = RTCPeerConnectionFactory()
    let configuration = RTCConfiguration()
    configuration.sdpSemantics = .unifiedPlan
    configuration.continualGatheringPolicy = .gatherContinually
    if let backupPingInterval, backupPingInterval > 0 {
        configuration.iceBackupCandidatePairPingInterval = backupPingInterval
        print("M144 backup candidate-pair ping interval: \(backupPingInterval) ms")
    }
    if let receivingTimeout, receivingTimeout > 0 {
        configuration.iceConnectionReceivingTimeout = receivingTimeout
        print("M144 ICE receiving timeout: \(receivingTimeout) ms")
    }
    if let strongCheckInterval, strongCheckInterval > 0 {
        configuration.iceCheckIntervalStrongConnectivity = NSNumber(value: strongCheckInterval)
        print("M144 strong-connectivity check interval: \(strongCheckInterval) ms")
    }
    if let weakCheckInterval, weakCheckInterval > 0 {
        configuration.iceCheckIntervalWeakConnectivity = NSNumber(value: weakCheckInterval)
        print("M144 weak-connectivity check interval: \(weakCheckInterval) ms")
    }
    if let minimumCheckInterval, minimumCheckInterval > 0 {
        configuration.iceCheckMinInterval = NSNumber(value: minimumCheckInterval)
        print("M144 minimum ICE check interval: \(minimumCheckInterval) ms")
    }
    if let unwritableTimeout, unwritableTimeout > 0 {
        configuration.iceUnwritableTimeout = NSNumber(value: unwritableTimeout)
        print("M144 ICE unwritable timeout: \(unwritableTimeout) ms")
    }
    if let unwritableMinChecks, unwritableMinChecks > 0 {
        configuration.iceUnwritableMinChecks = NSNumber(value: unwritableMinChecks)
        print("M144 ICE unwritable minimum checks: \(unwritableMinChecks)")
    }
    if let inactiveTimeout, inactiveTimeout > 0 {
        configuration.iceInactiveTimeout = NSNumber(value: inactiveTimeout)
        print("M144 ICE inactive timeout: \(inactiveTimeout) ms")
    }
    let constraints = RTCMediaConstraints(mandatoryConstraints: nil, optionalConstraints: nil)
    let peerDelegate = PeerDelegate()
    guard let peer = factory.peerConnection(
        with: configuration,
        constraints: constraints,
        delegate: peerDelegate
    ) else {
        throw ProbeError.webRTC("could not create RTCPeerConnection")
    }
    defer { peer.close() }

    if sendAudio {
        let audioTrack = factory.audioTrack(withTrackId: "m144-renomination-probe-audio")
        guard peer.add(audioTrack, streamIds: ["m144-renomination-probe"]) != nil else {
            throw ProbeError.webRTC("could not add local audio track")
        }
        print("M144 local audio track enabled")
    }

    try setRemoteDescription(
        RTCSessionDescription(type: .offer, sdp: offerSDP),
        on: peer
    )
    let answer = try createAnswer(on: peer, constraints: constraints)
    try setLocalDescription(answer, on: peer)

    guard peerDelegate.firstCandidate.wait(timeout: .now() + 10) == .success else {
        throw ProbeError.timeout("first ICE candidate")
    }
    // Continual gathering intentionally may never enter `complete`. Give the
    // initial interface scan a short window, then send the current bundled SDP.
    Thread.sleep(forTimeInterval: 1.0)
    guard let localSDP = peer.localDescription?.sdp else {
        throw ProbeError.webRTC("localDescription disappeared after gathering")
    }

    let answerOptions = iceOptions(in: localSDP)
    print("M144 answer ICE options: \(answerOptions)")
    guard answerOptions.contains(where: { $0.split(separator: ":", maxSplits: 1).last?.split(separator: " ").contains("renomination") == true }) else {
        throw ProbeError.webRTC("M144 did not negotiate renomination in its answer")
    }

    _ = try rpc.call(
        id: "3",
        method: "endpoint.webrtc.accept_answer",
        params: ["endpoint_id": endpointID, "sdp": localSDP]
    )

    guard peerDelegate.iceConnected.wait(timeout: .now() + 15) == .success else {
        throw ProbeError.timeout("ICE connected/completed")
    }

    if expectReplacement {
        _ = try rpc.call(
            id: "stats-subscribe",
            method: "stats.subscribe",
            params: ["interval_ms": 500]
        )
        _ = try rpc.call(
            id: "tone",
            method: "endpoint.create_tone",
            params: ["tone": "sine", "frequency": 1000]
        )
        Thread.sleep(forTimeInterval: 1.5)
        let beforeCounters = try rtpCounters(on: peer)
        print(
            "M144 RTP before fault: "
                + "in=\(beforeCounters.inboundPackets)/\(beforeCounters.inboundBytes) "
                + "out=\(beforeCounters.outboundPackets)/\(beforeCounters.outboundBytes) "
                + "ssrc=\(beforeCounters.inboundSsrcs)/\(beforeCounters.outboundSsrcs)"
        )
        guard beforeCounters.inboundPackets > 0 else {
            throw ProbeError.webRTC("no inbound tone RTP arrived before the path fault")
        }
        if sendAudio, beforeCounters.outboundPackets == 0 {
            throw ProbeError.webRTC("local audio track sent no RTP before the path fault")
        }

        let initialInfo = try rpc.call(id: "4", method: "session.info", params: [:])
        guard let bridgeBeforeCounters = rpc.endpointCounters(endpointID: endpointID) else {
            throw ProbeError.invalidResponse("rtpbridge emitted no endpoint stats before the path fault")
        }
        print(
            "rtpbridge RTP before fault: "
                + "in=\(bridgeBeforeCounters.inboundPackets)/\(bridgeBeforeCounters.inboundBytes) "
                + "out=\(bridgeBeforeCounters.outboundPackets)/\(bridgeBeforeCounters.outboundBytes)"
        )
        guard bridgeBeforeCounters.outboundPackets > 0 else {
            throw ProbeError.webRTC("rtpbridge sent no tone RTP before the path fault")
        }
        if sendAudio, bridgeBeforeCounters.inboundPackets == 0 {
            throw ProbeError.webRTC("rtpbridge received no M144 RTP before the path fault")
        }
        guard let initialRemote = remoteAddress(in: initialInfo, endpointID: endpointID) else {
            throw ProbeError.invalidResponse("session.info omitted the initial selected remote address")
        }
        guard let initialGeneration = offerGeneration(in: initialInfo, endpointID: endpointID) else {
            throw ProbeError.invalidResponse("session.info omitted the initial offer generation")
        }
        print("rtpbridge initial selected remote: \(initialRemote)")

        guard peerDelegate.replacementPairSelected.wait(timeout: .now() + 45) == .success else {
            throw ProbeError.timeout("M144 to select a replacement ICE pair")
        }

        var replacementRemote: String?
        var replacementGeneration: UInt64?
        for attempt in 0 ..< 20 {
            let info = try rpc.call(
                id: "pair-check-\(attempt)",
                method: "session.info",
                params: [:]
            )
            if let remote = remoteAddress(in: info, endpointID: endpointID), remote != initialRemote {
                replacementRemote = remote
                replacementGeneration = offerGeneration(in: info, endpointID: endpointID)
                break
            }
            Thread.sleep(forTimeInterval: 1.0)
        }
        guard let replacementRemote else {
            throw ProbeError.timeout("rtpbridge to accept the replacement nominated pair")
        }
        guard replacementGeneration == initialGeneration else {
            throw ProbeError.webRTC(
                "offer generation changed during re-nomination: "
                    + "\(initialGeneration) -> \(String(describing: replacementGeneration))"
            )
        }
        Thread.sleep(forTimeInterval: 1.5)
        _ = try rpc.call(id: "stats-after-switch", method: "session.info", params: [:])
        let afterCounters = try rtpCounters(on: peer)
        print(
            "M144 RTP after switch: "
                + "in=\(afterCounters.inboundPackets)/\(afterCounters.inboundBytes) "
                + "out=\(afterCounters.outboundPackets)/\(afterCounters.outboundBytes) "
                + "ssrc=\(afterCounters.inboundSsrcs)/\(afterCounters.outboundSsrcs)"
        )
        guard afterCounters.inboundPackets > beforeCounters.inboundPackets else {
            throw ProbeError.webRTC("inbound RTP did not continue on the replacement path")
        }
        if sendAudio, afterCounters.outboundPackets <= beforeCounters.outboundPackets {
            throw ProbeError.webRTC("outbound RTP did not continue on the replacement path")
        }
        guard afterCounters.inboundSsrcs == beforeCounters.inboundSsrcs else {
            throw ProbeError.webRTC("inbound SSRC changed across re-nomination")
        }
        if sendAudio, afterCounters.outboundSsrcs != beforeCounters.outboundSsrcs {
            throw ProbeError.webRTC("outbound SSRC changed across re-nomination")
        }
        guard let bridgeAfterCounters = rpc.endpointCounters(endpointID: endpointID) else {
            throw ProbeError.invalidResponse("rtpbridge emitted no endpoint stats after the path switch")
        }
        print(
            "rtpbridge RTP after switch: "
                + "in=\(bridgeAfterCounters.inboundPackets)/\(bridgeAfterCounters.inboundBytes) "
                + "out=\(bridgeAfterCounters.outboundPackets)/\(bridgeAfterCounters.outboundBytes)"
        )
        guard bridgeAfterCounters.outboundPackets > bridgeBeforeCounters.outboundPackets else {
            throw ProbeError.webRTC("rtpbridge outbound RTP did not continue on the replacement path")
        }
        if sendAudio, bridgeAfterCounters.inboundPackets <= bridgeBeforeCounters.inboundPackets {
            throw ProbeError.webRTC("rtpbridge inbound RTP did not continue on the replacement path")
        }
        print("rtpbridge replacement selected remote: \(replacementRemote)")
        print("PASS: M144 re-nominated a replacement ICE pair with continuous RTP and no ICE restart")
    } else {
        // Give the selected-pair callback and server trace log time to flush.
        Thread.sleep(forTimeInterval: 1.0)
        print("PASS: M144 negotiated renomination and established ICE without an ICE restart")
    }

    _ = try rpc.call(id: "destroy", method: "session.destroy", params: [:])
}

do {
    try runProbe()
} catch {
    fputs("FAIL: \(error)\n", stderr)
    exit(1)
}

// Connection to the bridge server in the XEarthLayer process.
//
// One Unix socket carries every request. Calls are tagged with an id and may
// be in flight together; a reader thread hands each response to the caller
// waiting on its id. If the connection drops (XEarthLayer quit or restarted),
// pending calls fail with EIO and the next call reconnects.

import Foundation
import os

final class BridgeClient: @unchecked Sendable {
    typealias Completion = (Result<BridgeReader, BridgeError>) -> Void

    private let socketPath: String
    private let log = Logger(subsystem: "net.defreyssi.xearthlayer.fskit", category: "bridge")
    private let lock = NSLock()
    private var fd: Int32 = -1
    private var nextID: UInt64 = 1
    private var pending: [UInt64: Completion] = [:]

    init(socketPath: String) {
        self.socketPath = socketPath
    }

    deinit {
        lock.lock()
        let fd = self.fd
        self.fd = -1
        lock.unlock()
        if fd >= 0 { close(fd) }
    }

    /// Connect now, so that a missing server fails the mount rather than the
    /// first file access.
    func connect() throws {
        lock.lock()
        defer { lock.unlock() }
        _ = try connectedLocked()
    }

    /// Send `request` and call `completion` with the response body, positioned
    /// after the errno, or with the error the server or connection reported.
    func call(_ request: BridgeRequest, completion: @escaping Completion) {
        lock.lock()
        let id = nextID
        nextID += 1
        let fd: Int32
        do {
            fd = try connectedLocked()
        } catch let error as BridgeError {
            lock.unlock()
            completion(.failure(error))
            return
        } catch {
            lock.unlock()
            completion(.failure(.disconnected))
            return
        }
        pending[id] = completion
        // Writes are serialised under the lock so frames never interleave.
        let ok = request.encode(id: id).withUnsafeBytes { writeAll(fd, $0) }
        if !ok {
            let failed = dropConnectionLocked()
            lock.unlock()
            failed.forEach { $0(.failure(.disconnected)) }
            return
        }
        lock.unlock()
    }

    /// Blocking form of `call`, for the few FSKit entry points that are
    /// synchronous properties.
    func callSync(_ request: BridgeRequest, timeout: TimeInterval) -> Result<BridgeReader, BridgeError> {
        let done = DispatchSemaphore(value: 0)
        var result: Result<BridgeReader, BridgeError> = .failure(.disconnected)
        call(request) { r in
            result = r
            done.signal()
        }
        if done.wait(timeout: .now() + timeout) == .timedOut {
            return .failure(.disconnected)
        }
        return result
    }

    // MARK: - Connection

    private func connectedLocked() throws -> Int32 {
        if fd >= 0 { return fd }
        let sock = socket(AF_UNIX, SOCK_STREAM, 0)
        guard sock >= 0 else { throw BridgeError.posix(errno) }

        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let pathBytes = Array(socketPath.utf8)
        guard pathBytes.count < MemoryLayout.size(ofValue: addr.sun_path) else {
            close(sock)
            throw BridgeError.posix(ENAMETOOLONG)
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: pathBytes)
            raw[pathBytes.count] = 0
        }
        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(sock, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard rc == 0 else {
            let err = errno
            close(sock)
            log.error("connect \(self.socketPath, privacy: .public) failed: \(err)")
            throw BridgeError.posix(err)
        }
        var noSigPipe: Int32 = 1
        setsockopt(sock, SOL_SOCKET, SO_NOSIGPIPE, &noSigPipe, socklen_t(MemoryLayout<Int32>.size))
        fd = sock
        let reader = Thread { [weak self] in self?.readLoop(sock) }
        reader.name = "xearthlayer-bridge-reader"
        reader.start()
        log.info("connected to \(self.socketPath, privacy: .public)")
        return sock
    }

    /// Forget the connection and return the completions that were waiting on it.
    private func dropConnectionLocked() -> [Completion] {
        if fd >= 0 {
            shutdown(fd, SHUT_RDWR)
            close(fd)
            fd = -1
        }
        let failed = Array(pending.values)
        pending.removeAll()
        return failed
    }

    private func readLoop(_ sock: Int32) {
        while true {
            guard let header = readExact(sock, 4) else { break }
            let length = BridgeReader(header).leUInt32()
            guard length >= 12, length <= 64 << 20, let body = readExact(sock, Int(length)) else { break }
            var reader = BridgeReader(body)
            guard let id = try? reader.le(UInt64.self), let status = try? reader.le(Int32.self) else { break }
            lock.lock()
            let completion = pending.removeValue(forKey: id)
            lock.unlock()
            completion?(status == 0 ? .success(reader) : .failure(.posix(status)))
        }
        lock.lock()
        // Only tear down if this is still the live connection.
        let failed = fd == sock ? dropConnectionLocked() : []
        lock.unlock()
        failed.forEach { $0(.failure(.disconnected)) }
    }
}

private func writeAll(_ fd: Int32, _ buffer: UnsafeRawBufferPointer) -> Bool {
    var sent = 0
    while sent < buffer.count {
        let n = write(fd, buffer.baseAddress! + sent, buffer.count - sent)
        if n < 0 && errno == EINTR { continue }
        guard n > 0 else { return false }
        sent += n
    }
    return true
}

private func readExact(_ fd: Int32, _ count: Int) -> Data? {
    var data = Data(count: count)
    var got = 0
    let ok = data.withUnsafeMutableBytes { raw -> Bool in
        while got < count {
            let n = read(fd, raw.baseAddress! + got, count - got)
            if n < 0 && errno == EINTR { continue }
            guard n > 0 else { return false }
            got += n
        }
        return true
    }
    return ok ? data : nil
}

private extension BridgeReader {
    func leUInt32() -> UInt32 {
        var copy = self
        return (try? copy.le(UInt32.self)) ?? 0
    }
}

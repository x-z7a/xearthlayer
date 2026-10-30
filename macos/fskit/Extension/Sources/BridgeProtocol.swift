// Wire format spoken with the XEarthLayer bridge server.
//
// Mirrors xearthlayer/src/fuse/bridge/protocol.rs; a change to either must be
// made to both. Frames are a little-endian UInt32 body length followed by the
// body. Requests: UInt64 id, UInt8 opcode, payload. Responses: UInt64 id,
// Int32 errno (0 on success), payload only on success. Item ids are FSKit's:
// 1 is the root's parent, 2 the root.

import Foundation

enum BridgeRequest {
    case lookup(parent: UInt64, name: Data)
    case getAttr(itemID: UInt64)
    case readDir(itemID: UInt64, cookie: UInt64, withAttrs: Bool)
    case open(itemID: UInt64)
    case read(itemID: UInt64, fh: UInt64, offset: UInt64, length: UInt32)
    case release(itemID: UInt64, fh: UInt64)
    case statFs

    func encode(id: UInt64) -> Data {
        var body = Data()
        body.appendLE(id)
        switch self {
        case let .lookup(parent, name):
            body.append(1)
            body.appendLE(parent)
            body.appendLE(UInt16(name.count))
            body.append(name)
        case let .getAttr(itemID):
            body.append(2)
            body.appendLE(itemID)
        case let .readDir(itemID, cookie, withAttrs):
            body.append(3)
            body.appendLE(itemID)
            body.appendLE(cookie)
            body.append(withAttrs ? 1 : 0)
        case let .open(itemID):
            body.append(4)
            body.appendLE(itemID)
        case let .read(itemID, fh, offset, length):
            body.append(5)
            body.appendLE(itemID)
            body.appendLE(fh)
            body.appendLE(offset)
            body.appendLE(length)
        case let .release(itemID, fh):
            body.append(6)
            body.appendLE(itemID)
            body.appendLE(fh)
        case .statFs:
            body.append(7)
        }
        var frame = Data()
        frame.appendLE(UInt32(body.count))
        frame.append(body)
        return frame
    }
}

struct BridgeTime {
    var sec: Int64
    var nsec: UInt32

    var timespec: timespec {
        Darwin.timespec(tv_sec: Int(sec), tv_nsec: Int(nsec))
    }
}

/// FSKit's FSItemType numbering.
enum BridgeItemKind: UInt8 {
    case unknown = 0, file = 1, directory = 2, symlink = 3
}

struct BridgeAttr {
    var itemID: UInt64
    var parentID: UInt64
    var size: UInt64
    var allocSize: UInt64
    var kind: BridgeItemKind
    var mode: UInt16
    var nlink: UInt32
    var uid: UInt32
    var gid: UInt32
    var atime: BridgeTime
    var mtime: BridgeTime
    var ctime: BridgeTime
    var birthtime: BridgeTime
}

struct BridgeDirEntry {
    var itemID: UInt64
    var kind: BridgeItemKind
    var nextCookie: UInt64
    var name: Data
    var attr: BridgeAttr?
}

struct BridgeStatFs {
    var blockSize: UInt32
    var totalBlocks: UInt64
    var freeBlocks: UInt64
    var availableBlocks: UInt64
    var totalFiles: UInt64
    var freeFiles: UInt64
}

enum BridgeError: Error {
    case malformed(String)
    case disconnected
    case posix(Int32)
}

/// Sequential little-endian reader over a response body.
struct BridgeReader {
    private let data: Data
    private var offset: Int

    init(_ data: Data) {
        self.data = data
        offset = data.startIndex
    }

    var remaining: Int { data.endIndex - offset }

    mutating func bytes(_ count: Int) throws -> Data {
        guard remaining >= count else {
            throw BridgeError.malformed("wanted \(count) bytes, \(remaining) left")
        }
        let slice = data[offset..<offset + count]
        offset += count
        return slice
    }

    mutating func rest() -> Data {
        let slice = data[offset..<data.endIndex]
        offset = data.endIndex
        return slice
    }

    mutating func le<T: FixedWidthInteger>(_: T.Type) throws -> T {
        let raw = try bytes(MemoryLayout<T>.size)
        var value: T = 0
        for (shift, byte) in raw.enumerated() {
            value |= T(truncatingIfNeeded: byte) << (shift * 8)
        }
        return value
    }

    mutating func time() throws -> BridgeTime {
        BridgeTime(sec: try le(Int64.self), nsec: try le(UInt32.self))
    }

    mutating func kind() throws -> BridgeItemKind {
        let raw = try le(UInt8.self)
        guard let kind = BridgeItemKind(rawValue: raw) else {
            throw BridgeError.malformed("unknown item kind \(raw)")
        }
        return kind
    }

    mutating func attr() throws -> BridgeAttr {
        BridgeAttr(
            itemID: try le(UInt64.self),
            parentID: try le(UInt64.self),
            size: try le(UInt64.self),
            allocSize: try le(UInt64.self),
            kind: try kind(),
            mode: try le(UInt16.self),
            nlink: try le(UInt32.self),
            uid: try le(UInt32.self),
            gid: try le(UInt32.self),
            atime: try time(),
            mtime: try time(),
            ctime: try time(),
            birthtime: try time()
        )
    }

    mutating func dirEntries() throws -> [BridgeDirEntry] {
        let count = try le(UInt32.self)
        var entries: [BridgeDirEntry] = []
        entries.reserveCapacity(Int(min(count, 512)))
        for _ in 0..<count {
            let itemID = try le(UInt64.self)
            let kind = try kind()
            let nextCookie = try le(UInt64.self)
            let nameLength = Int(try le(UInt16.self))
            let name = try bytes(nameLength)
            let attr = try le(UInt8.self) != 0 ? try self.attr() : nil
            entries.append(BridgeDirEntry(
                itemID: itemID, kind: kind, nextCookie: nextCookie, name: name, attr: attr))
        }
        return entries
    }

    mutating func statFs() throws -> BridgeStatFs {
        BridgeStatFs(
            blockSize: try le(UInt32.self),
            totalBlocks: try le(UInt64.self),
            freeBlocks: try le(UInt64.self),
            availableBlocks: try le(UInt64.self),
            totalFiles: try le(UInt64.self),
            freeFiles: try le(UInt64.self)
        )
    }
}

extension Data {
    mutating func appendLE<T: FixedWidthInteger>(_ value: T) {
        Swift.withUnsafeBytes(of: value.littleEndian) { append(contentsOf: $0) }
    }
}

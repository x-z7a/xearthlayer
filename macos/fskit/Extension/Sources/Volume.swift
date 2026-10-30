// The read-only XEarthLayer volume: every operation is a bridge call.

import FSKit
import Foundation
import os

/// One file or directory. FSKit wants the same object back for the same
/// item, so the volume keeps them by id until FSKit reclaims them.
final class BridgeItem: FSItem, @unchecked Sendable {
    let itemID: UInt64
    let lock = NSLock()
    var parentID: UInt64
    var attr: BridgeAttr
    /// Bridge handle while the file is open; reads go through it so the
    /// server resolves a tile once per open rather than once per read.
    var fh: UInt64?

    init(attr: BridgeAttr, parentID: UInt64) {
        itemID = attr.itemID
        self.parentID = parentID
        self.attr = attr
    }
}

final class XEarthLayerVolume: FSVolume, @unchecked Sendable {
    private let bridge: BridgeClient
    private let log = Logger(subsystem: "net.defreyssi.xearthlayer.fskit", category: "volume")
    private let itemsLock = NSLock()
    private var items: [UInt64: BridgeItem] = [:]

    /// FSKit's id for the root and for the root's parent.
    private static let rootID: UInt64 = 2
    private static let rootParentID: UInt64 = 1

    init(volumeID: FSVolume.Identifier, volumeName: FSFileName, bridge: BridgeClient) {
        self.bridge = bridge
        super.init(volumeID: volumeID, volumeName: volumeName)
    }

    /// The cached item for `attr.itemID`, refreshed, or a new one.
    private func item(for attr: BridgeAttr, parentID: UInt64) -> BridgeItem {
        itemsLock.lock()
        defer { itemsLock.unlock() }
        if let existing = items[attr.itemID] {
            existing.lock.lock()
            existing.attr = attr
            if parentID != 0 { existing.parentID = parentID }
            existing.lock.unlock()
            return existing
        }
        let created = BridgeItem(attr: attr, parentID: parentID)
        items[attr.itemID] = created
        return created
    }

    private func bridgeItem(_ item: FSItem) -> BridgeItem? {
        item as? BridgeItem
    }
}

// MARK: - Errors

private func nsError(_ error: BridgeError) -> any Error {
    switch error {
    case let .posix(code): return fs_errorForPOSIXError(code)
    case .disconnected, .malformed: return fs_errorForPOSIXError(EIO)
    }
}

private let readOnly = fs_errorForPOSIXError(EROFS)

// MARK: - Attributes

private extension BridgeItemKind {
    var fsType: FSItem.ItemType {
        switch self {
        case .file: return .file
        case .directory: return .directory
        case .symlink: return .symlink
        case .unknown: return .unknown
        }
    }
}

private func fsAttributes(_ attr: BridgeAttr, parentID: UInt64) -> FSItem.Attributes {
    let out = FSItem.Attributes()
    out.type = attr.kind.fsType
    out.mode = UInt32(attr.mode)
    out.linkCount = attr.nlink
    out.uid = attr.uid
    out.gid = attr.gid
    out.flags = 0
    out.size = attr.size
    out.allocSize = attr.allocSize
    out.fileID = FSItem.Identifier(rawValue: attr.itemID) ?? .invalid
    let parent = attr.parentID != 0 ? attr.parentID : parentID
    out.parentID = FSItem.Identifier(rawValue: parent) ?? .invalid
    out.accessTime = attr.atime.timespec
    out.modifyTime = attr.mtime.timespec
    out.changeTime = attr.ctime.timespec
    out.birthTime = attr.birthtime.timespec
    return out
}

// MARK: - Volume operations

extension XEarthLayerVolume: FSVolume.Operations {
    var supportedVolumeCapabilities: FSVolume.SupportedCapabilities {
        let caps = FSVolume.SupportedCapabilities()
        caps.supportsPersistentObjectIDs = false
        caps.supports64BitObjectIDs = true
        caps.supportsHardLinks = false
        caps.supportsSymbolicLinks = false
        caps.supportsSparseFiles = false
        caps.supports2TBFiles = true
        caps.supportsFastStatFS = false
        caps.doesNotSupportSettingFilePermissions = true
        caps.caseFormat = .sensitive
        return caps
    }

    var volumeStatistics: FSStatFSResult {
        let result = FSStatFSResult(fileSystemTypeName: "xearthlayer")
        guard case var .success(reader) = bridge.callSync(.statFs, timeout: 2),
              let stat = try? reader.statFs()
        else { return result }
        result.blockSize = Int(stat.blockSize)
        result.ioSize = 1 << 20
        result.totalBlocks = stat.totalBlocks
        result.freeBlocks = stat.freeBlocks
        result.availableBlocks = stat.availableBlocks
        result.usedBlocks = stat.totalBlocks - min(stat.freeBlocks, stat.totalBlocks)
        result.totalBytes = stat.totalBlocks * UInt64(stat.blockSize)
        result.freeBytes = stat.freeBlocks * UInt64(stat.blockSize)
        result.availableBytes = stat.availableBlocks * UInt64(stat.blockSize)
        result.usedBytes = result.totalBytes - min(result.freeBytes, result.totalBytes)
        result.totalFiles = stat.totalFiles
        result.freeFiles = stat.freeFiles
        return result
    }

    func activate(options: FSTaskOptions, replyHandler: @escaping (FSItem?, (any Error)?) -> Void) {
        bridge.call(.getAttr(itemID: Self.rootID)) { [self] result in
            switch result {
            case var .success(reader):
                do {
                    let attr = try reader.attr()
                    replyHandler(item(for: attr, parentID: Self.rootParentID), nil)
                } catch {
                    replyHandler(nil, fs_errorForPOSIXError(EIO))
                }
            case let .failure(error):
                log.error("activate: root getattr failed: \(String(describing: error), privacy: .public)")
                replyHandler(nil, nsError(error))
            }
        }
    }

    func deactivate(options: FSDeactivateOptions = [], replyHandler: @escaping ((any Error)?) -> Void) {
        itemsLock.lock()
        items.removeAll()
        itemsLock.unlock()
        replyHandler(nil)
    }

    func mount(options: FSTaskOptions, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func unmount(replyHandler: @escaping () -> Void) {
        replyHandler()
    }

    func synchronize(flags: FSSyncFlags, replyHandler: @escaping ((any Error)?) -> Void) {
        replyHandler(nil)
    }

    func getAttributes(
        _ desiredAttributes: FSItem.GetAttributesRequest,
        of item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        guard let item = bridgeItem(item) else {
            replyHandler(nil, fs_errorForPOSIXError(EINVAL))
            return
        }
        // Attributes arrive with every lookup and directory page and do not
        // change during a session, so the cached copy is current.
        item.lock.lock()
        let attr = item.attr
        let parentID = item.parentID
        item.lock.unlock()
        replyHandler(fsAttributes(attr, parentID: parentID), nil)
    }

    func setAttributes(
        _ newAttributes: FSItem.SetAttributesRequest,
        on item: FSItem,
        replyHandler: @escaping (FSItem.Attributes?, (any Error)?) -> Void
    ) {
        replyHandler(nil, readOnly)
    }

    func lookupItem(
        named name: FSFileName,
        inDirectory directory: FSItem,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        guard let directory = bridgeItem(directory) else {
            replyHandler(nil, nil, fs_errorForPOSIXError(EINVAL))
            return
        }
        bridge.call(.lookup(parent: directory.itemID, name: name.data)) { [self] result in
            switch result {
            case var .success(reader):
                guard let attr = try? reader.attr() else {
                    replyHandler(nil, nil, fs_errorForPOSIXError(EIO))
                    return
                }
                replyHandler(item(for: attr, parentID: directory.itemID), name, nil)
            case let .failure(error):
                replyHandler(nil, nil, nsError(error))
            }
        }
    }

    func reclaimItem(_ item: FSItem, replyHandler: @escaping ((any Error)?) -> Void) {
        if let item = bridgeItem(item) {
            itemsLock.lock()
            if items[item.itemID] === item { items.removeValue(forKey: item.itemID) }
            itemsLock.unlock()
            releaseHandle(of: item)
        }
        replyHandler(nil)
    }

    func readSymbolicLink(_ item: FSItem, replyHandler: @escaping (FSFileName?, (any Error)?) -> Void) {
        replyHandler(nil, fs_errorForPOSIXError(EINVAL))
    }

    func createItem(
        named name: FSFileName,
        type: FSItem.ItemType,
        inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, nil, readOnly)
    }

    func createSymbolicLink(
        named name: FSFileName,
        inDirectory directory: FSItem,
        attributes newAttributes: FSItem.SetAttributesRequest,
        linkContents contents: FSFileName,
        replyHandler: @escaping (FSItem?, FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, nil, readOnly)
    }

    func createLink(
        to item: FSItem,
        named name: FSFileName,
        inDirectory directory: FSItem,
        replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, readOnly)
    }

    func removeItem(
        _ item: FSItem,
        named name: FSFileName,
        fromDirectory directory: FSItem,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        replyHandler(readOnly)
    }

    func renameItem(
        _ item: FSItem,
        inDirectory sourceDirectory: FSItem,
        named sourceName: FSFileName,
        to destinationName: FSFileName,
        inDirectory destinationDirectory: FSItem,
        overItem: FSItem?,
        replyHandler: @escaping (FSFileName?, (any Error)?) -> Void
    ) {
        replyHandler(nil, readOnly)
    }

    func enumerateDirectory(
        _ directory: FSItem,
        startingAt cookie: FSDirectoryCookie,
        verifier: FSDirectoryVerifier,
        attributes: FSItem.GetAttributesRequest?,
        packer: FSDirectoryEntryPacker,
        replyHandler: @escaping (FSDirectoryVerifier, (any Error)?) -> Void
    ) {
        guard let directory = bridgeItem(directory) else {
            replyHandler(verifier, fs_errorForPOSIXError(EINVAL))
            return
        }
        // The directory tree is fixed for the life of the mount.
        let current = FSDirectoryVerifier(rawValue: 1)
        bridge.call(.readDir(itemID: directory.itemID, cookie: cookie.rawValue, withAttrs: attributes != nil)) { result in
            switch result {
            case var .success(reader):
                guard let entries = try? reader.dirEntries() else {
                    replyHandler(current, fs_errorForPOSIXError(EIO))
                    return
                }
                for entry in entries {
                    let attrs = entry.attr.map { fsAttributes($0, parentID: directory.itemID) }
                    let packed = packer.packEntry(
                        name: FSFileName(data: entry.name),
                        itemType: entry.kind.fsType,
                        itemID: FSItem.Identifier(rawValue: entry.itemID) ?? .invalid,
                        nextCookie: FSDirectoryCookie(rawValue: entry.nextCookie),
                        attributes: attrs)
                    if !packed { break }
                }
                replyHandler(current, nil)
            case let .failure(error):
                replyHandler(current, nsError(error))
            }
        }
    }

    // MARK: - Path configuration

    var maximumLinkCount: Int { 1 }
    var maximumNameLength: Int { 255 }
    var restrictsOwnershipChanges: Bool { true }
    var truncatesLongNames: Bool { false }
    var maximumXattrSize: Int { 0 }
    var maximumFileSize: UInt64 { UInt64(Int64.max) }
}

// MARK: - Open and close

extension XEarthLayerVolume: FSVolume.OpenCloseOperations {
    func openItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void) {
        guard let item = bridgeItem(item) else {
            replyHandler(fs_errorForPOSIXError(EINVAL))
            return
        }
        if modes.contains(.write) {
            replyHandler(readOnly)
            return
        }
        item.lock.lock()
        let isFile = item.attr.kind == .file
        let alreadyOpen = item.fh != nil
        item.lock.unlock()
        guard isFile, !alreadyOpen else {
            replyHandler(nil)
            return
        }
        bridge.call(.open(itemID: item.itemID)) { [self] result in
            switch result {
            case var .success(reader):
                let fh = (try? reader.le(UInt64.self)) ?? 0
                item.lock.lock()
                let raced = item.fh != nil
                if !raced { item.fh = fh }
                item.lock.unlock()
                // Another open won; give this handle straight back.
                if raced, fh != 0 {
                    bridge.call(.release(itemID: item.itemID, fh: fh)) { _ in }
                }
                replyHandler(nil)
            case let .failure(error):
                replyHandler(nsError(error))
            }
        }
    }

    func closeItem(_ item: FSItem, modes: FSVolume.OpenModes, replyHandler: @escaping ((any Error)?) -> Void) {
        // `modes` is what stays open; empty means this was the last close.
        if modes.isEmpty, let item = bridgeItem(item) {
            releaseHandle(of: item)
        }
        replyHandler(nil)
    }

    fileprivate func releaseHandle(of item: BridgeItem) {
        item.lock.lock()
        let fh = item.fh
        item.fh = nil
        item.lock.unlock()
        if let fh, fh != 0 {
            bridge.call(.release(itemID: item.itemID, fh: fh)) { _ in }
        }
    }
}

// MARK: - Read

extension XEarthLayerVolume: FSVolume.ReadWriteOperations {
    func read(
        from item: FSItem,
        at offset: off_t,
        length: Int,
        into buffer: FSMutableFileDataBuffer,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        guard let item = bridgeItem(item) else {
            replyHandler(0, fs_errorForPOSIXError(EINVAL))
            return
        }
        item.lock.lock()
        let fh = item.fh ?? 0
        item.lock.unlock()
        let wanted = UInt32(clamping: min(length, buffer.length))
        bridge.call(.read(itemID: item.itemID, fh: fh, offset: UInt64(offset), length: wanted)) { result in
            switch result {
            case var .success(reader):
                let data = reader.rest()
                let copied = buffer.withUnsafeMutableBytes { dest -> Int in
                    let n = min(data.count, dest.count)
                    data.copyBytes(to: dest.bindMemory(to: UInt8.self), count: n)
                    return n
                }
                replyHandler(copied, nil)
            case let .failure(error):
                replyHandler(0, nsError(error))
            }
        }
    }

    func write(
        contents: Data,
        to item: FSItem,
        at offset: off_t,
        replyHandler: @escaping (Int, (any Error)?) -> Void
    ) {
        replyHandler(0, readOnly)
    }
}

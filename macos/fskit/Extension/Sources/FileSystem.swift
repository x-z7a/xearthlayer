// FSKit module entry point and resource handling.
//
// The resource is a directory XEarthLayer creates per mount, passed with
// `mount -F -t xearthlayer <dir> <mountpoint>`. It holds the bridge socket;
// its name is the volume name.

import ExtensionFoundation
import FSKit
import Foundation
import os

@main
struct XEarthLayerFSExtension: UnaryFileSystemExtension {
    let fileSystem = XEarthLayerFileSystem()
}

/// Name of the socket inside the resource directory (`SOCKET_NAME` in
/// xearthlayer/src/fuse/fskit/mount.rs).
let bridgeSocketName = "bridge.sock"

final class XEarthLayerFileSystem: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    private let log = Logger(subsystem: "net.defreyssi.xearthlayer.fskit", category: "filesystem")

    func probeResource(
        resource: FSResource,
        replyHandler: @escaping (FSProbeResult?, (any Error)?) -> Void
    ) {
        guard let url = Self.resourceURL(resource) else {
            replyHandler(FSProbeResult.notRecognized, nil)
            return
        }
        let socket = url.appendingPathComponent(bridgeSocketName).path
        guard FileManager.default.fileExists(atPath: socket) else {
            log.error("probe: no bridge socket at \(socket, privacy: .public)")
            replyHandler(FSProbeResult.notRecognized, nil)
            return
        }
        replyHandler(
            FSProbeResult.usable(name: url.lastPathComponent, containerID: Self.containerID(for: url)),
            nil)
    }

    func loadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping (FSVolume?, (any Error)?) -> Void
    ) {
        guard let url = Self.resourceURL(resource) else {
            replyHandler(nil, fs_errorForPOSIXError(EINVAL))
            return
        }
        _ = url.startAccessingSecurityScopedResource()
        let client = BridgeClient(socketPath: url.appendingPathComponent(bridgeSocketName).path)
        do {
            try client.connect()
        } catch {
            log.error("load: cannot reach XEarthLayer: \(String(describing: error), privacy: .public)")
            replyHandler(nil, fs_errorForPOSIXError(ECONNREFUSED))
            return
        }
        let volume = XEarthLayerVolume(
            volumeID: FSVolume.Identifier(uuid: Self.containerID(for: url).uuid),
            volumeName: FSFileName(string: url.lastPathComponent),
            bridge: client)
        containerStatus = .ready
        log.info("loaded \(url.path, privacy: .public)")
        replyHandler(volume, nil)
    }

    func unloadResource(
        resource: FSResource,
        options: FSTaskOptions,
        replyHandler: @escaping ((any Error)?) -> Void
    ) {
        Self.resourceURL(resource)?.stopAccessingSecurityScopedResource()
        replyHandler(nil)
    }

    private static func resourceURL(_ resource: FSResource) -> URL? {
        (resource as? FSPathURLResource)?.url
    }

    /// A stable id per resource path, so remounting the same folder is the
    /// same container.
    private static func containerID(for url: URL) -> FSContainerIdentifier {
        FSContainerIdentifier(uuid: UUID(namespaced: url.standardizedFileURL.path))
    }
}

extension UUID {
    /// A deterministic UUID derived from `name` (not RFC 4122 v5, but stable).
    init(namespaced name: String) {
        var hasher = [UInt8](repeating: 0, count: 16)
        for (i, byte) in name.utf8.enumerated() {
            hasher[i % 16] = hasher[i % 16] &* 31 &+ byte
        }
        hasher[6] = (hasher[6] & 0x0F) | 0x40
        hasher[8] = (hasher[8] & 0x3F) | 0x80
        self = NSUUID(uuidBytes: hasher) as UUID
    }
}

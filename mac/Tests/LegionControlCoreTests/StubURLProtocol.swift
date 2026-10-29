import Foundation

/// Canned HTTP answers, so the download code can be run end to end without the network.
///
/// Only sessions made by `session()` use it. Routes are kept by absolute URL and every test uses
/// URLs of its own, so tests running side by side never answer each other's requests.
final class StubURLProtocol: URLProtocol, @unchecked Sendable {
    struct Route: Sendable {
        var status: Int = 200
        var headers: [String: String] = [:]
        /// Delivered one after another, so a reader that stops part way has something to stop in.
        var chunks: [Data]
    }

    private static let lock = NSLock()
    nonisolated(unsafe) private static var routes: [String: Route] = [:]
    nonisolated(unsafe) private static var requested: [String] = []

    static func session() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [StubURLProtocol.self]
        return URLSession(configuration: configuration)
    }

    static func serve(_ url: URL, _ route: Route) {
        lock.withLock { routes[url.absoluteString] = route }
    }

    static func forget(_ urls: [URL]) {
        lock.withLock { for url in urls { routes[url.absoluteString] = nil } }
    }

    /// Whether anything asked for `url` since the routes were set up.
    static func wasRequested(_ url: URL) -> Bool {
        lock.withLock { requested.contains(url.absoluteString) }
    }

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        guard let url = request.url else { return }
        let route = Self.lock.withLock {
            Self.requested.append(url.absoluteString)
            return Self.routes[url.absoluteString]
        }
        guard let route,
              let response = HTTPURLResponse(url: url, statusCode: route.status, httpVersion: "HTTP/1.1",
                                             headerFields: route.headers)
        else {
            client?.urlProtocol(self, didFailWithError: URLError(.cannotConnectToHost))
            return
        }
        client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
        for chunk in route.chunks { client?.urlProtocol(self, didLoad: chunk) }
        client?.urlProtocolDidFinishLoading(self)
    }

    override func stopLoading() {}
}

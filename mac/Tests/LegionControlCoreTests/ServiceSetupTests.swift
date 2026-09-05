import Foundation
import Testing
@testable import LegionControlCore

@MainActor
struct ServiceSetupTests {
    @Test("service edits preserve the whole document and saving requires the exact validated draft")
    func validatedDraftAndCAS() async throws {
        var sent: [AgentRequest] = []
        let model = ServiceSetupModel { request in
            sent.append(request)
            let verb = request.arguments[1]
            if verb == "get" {
                return try JSONDecoder().decode(ServiceConfigReply.self, from: Data("""
                {"ok":true,"hash":"\(String(repeating: "a", count: 64))","document":{"configVersion":3,"future":{"keep":true},"services":[{"id":"demo","name":"Old","kind":"command","futureService":42}]},"templates":[]}
                """.utf8))
            }
            let input = try #require(request.input)
            let root = try #require(try JSONSerialization.jsonObject(with: input) as? [String: Any])
            #expect(root["expectedHash"] as? String == String(repeating: "a", count: 64))
            let document = try #require(root["document"] as? [String: Any])
            #expect((document["future"] as? [String: Bool])?["keep"] == true)
            return try JSONDecoder().decode(ServiceConfigReply.self, from: Data("""
            {"ok":true,"valid":true,"saved":\(verb == "set"),"hash":"\(String(repeating: "b", count: 64))","changes":{"added":[],"removed":[],"changed":["demo"]},"warnings":[]}
            """.utf8))
        }
        await model.load()
        model.rename("demo", name: "Renamed")
        #expect(!model.canSave)
        await model.save()
        #expect(sent.count == 1)
        await model.validate()
        #expect(model.canSave)
        model.text += " "
        #expect(!model.canSave)
        await model.validate()
        await model.save()
        #expect(model.saved)
        #expect(model.expectedHash == String(repeating: "b", count: 64))
        #expect(sent.last?.arguments == ["service-config", "set", "--stdin"])
        let root = try model.parse()
        #expect((root["services"] as? [[String: Any]])?.first?["futureService"] as? Int == 42)
    }

    @Test("AI tool drafts preserve the document and require explicit scheduling opt-in")
    func aiProfiles() async throws {
        let model = ServiceSetupModel { _ in
            try JSONDecoder().decode(ServiceConfigReply.self, from: Data(#"{"ok":true,"hash":"abc","document":{"future":{"keep":true},"services":[]},"profiles":[{"id":"codex-desktop","name":"Codex","platform":"mac","detected":true,"availability":"available","message":"Installed","updateMethod":"sparkle","service":{"id":"codex-desktop","name":"Codex","kind":"app","updates":{"automatic":true,"future":42}}},{"id":"manual","name":"Manual tool","platform":"mac","detected":true,"availability":"manual","message":"Update manually","service":null}]}"#.utf8))
        }
        await model.load()
        #expect(model.profiles.count == 2)
        #expect(!model.profiles[1].canAdd)
        model.addProfile("manual")
        #expect(model.services.isEmpty)
        model.addProfile("codex-desktop")
        let root = try model.parse()
        let services = try #require(root["services"] as? [[String: Any]])
        let updates = try #require(services.first?["updates"] as? [String: Any])
        #expect(updates["automatic"] as? Bool == false)
        #expect(updates["future"] as? Int == 42)
        #expect((root["future"] as? [String: Bool])?["keep"] == true)
        #expect(!model.canSave)
        model.addProfile("codex-desktop")
        #expect(model.services.count == 1)
    }

    @Test("detected manual desktop drafts can be monitored without enabling updates")
    func monitorProfile() async throws {
        let model = ServiceSetupModel { _ in
            try JSONDecoder().decode(ServiceConfigReply.self, from: Data(#"{"ok":true,"hash":"abc","document":{"services":[]},"profiles":[{"id":"desktop","name":"Desktop app","platform":"mac","detected":true,"availability":"manual","message":"Updates managed by application","service":{"id":"desktop","kind":"command","commands":{"installedVersion":["/usr/bin/true"]},"updates":{"automatic":false}}}]}"#.utf8))
        }
        await model.load()
        #expect(model.profiles[0].canMonitor)
        model.addProfile("desktop")
        let document = try model.parse()
        let services = try #require(document["services"] as? [[String: Any]])
        #expect((services[0]["updates"] as? [String: Bool])?["automatic"] == false)
        #expect((services[0]["commands"] as? [String: Any])?["update"] == nil)
        #expect(!model.canSave)
        var absent = model.profiles[0]
        absent.detected = false
        #expect(!absent.canAdd)
        let status = try JSONDecoder().decode(ServiceStatus.self, from: Data(#"{"id":"desktop","name":"Desktop app","installed":"1","canUpdate":false}"#.utf8))
        #expect(!status.canBeUpdated)
    }

    @Test("restricted service setup remains read-only and never falls back to another command")
    func restrictedRefusal() async throws {
        var calls = 0
        let model = ServiceSetupModel { _ in
            calls += 1
            return try JSONDecoder().decode(ServiceConfigReply.self, from: FixtureTests.bytes("service-config.restricted.json"))
        }
        await model.load()
        #expect(model.restricted)
        #expect(!model.canSave)
        await model.save()
        #expect(calls == 1)
    }
}

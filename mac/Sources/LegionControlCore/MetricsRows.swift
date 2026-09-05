import SwiftUI

struct AgentMetric: Decodable, Sendable, Equatable, Identifiable {
    var id: String
    var name: String
    var value: Double?
    var unit: String
    var checkedAt: String?
    var error: String?
}

struct MetricsRows: View {
    var metrics: [AgentMetric]?
    var body: some View {
        if let metrics, !metrics.isEmpty {
            SectionHeading(title: "Metrics")
            VStack(alignment: .leading, spacing: 10) {
                ForEach(metrics) { metric in
                    DetailRow(label: metric.name) {
                        if let value = metric.value {
                            Text("\(value.formatted(.number.precision(.fractionLength(0...2)))) \(metric.unit)")
                        } else {
                            Text("Unavailable").foregroundStyle(.secondary)
                        }
                    }
                    if let error = metric.error, !error.isEmpty {
                        DisclosureGroup("Details") {
                            Text(error).font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                    }
                }
            }
        }
    }
}

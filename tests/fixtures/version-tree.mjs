// A checkout cut down to the files scripts/check-versions.mjs reads, with every version in step.
//
// Each file is reduced to the lines that carry a version or look the agent bundle up, written the
// way the real file writes them. The real files are checked by the test that runs on this
// repository, so a change to one of them fails that test and not every test built on this tree.
import fs from 'node:fs';
import path from 'node:path';

export function versionTree(root, { agent = '3.1.0', floor = agent, app = '1.3.4', build = 14 } = {}) {
  const write = (file, text) => {
    fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    fs.writeFileSync(path.join(root, file), text);
  };
  const legionControl = floor === null ? {} : { legionControl: { agentFloor: floor } };
  write('agent/package.json', JSON.stringify({ name: 'legionctl', version: agent, private: true, type: 'module', ...legionControl }, null, 2) + '\n');
  write('agent/src/config.mjs', `import fs from 'node:fs';

export const AGENT_VERSION = JSON.parse(fs.readFileSync(new URL('../package.json', import.meta.url), 'utf8')).version;
`);
  write('contract/tools/make-fixtures.mjs', `const AGENT_PACKAGE = JSON.parse(fs.readFileSync(path.join(HERE, '..', '..', 'agent', 'package.json'), 'utf8'));
const AGENT = AGENT_PACKAGE.version;
const AGENT_FLOOR = AGENT_PACKAGE.legionControl.agentFloor;
`);
  write('mac/Sources/LegionControlCore/AgentContract.swift', `/// The agent version bundled with this build, for the trusted deployment action.
let bundledAgentVersion = "${agent}"
`);
  write('desktop/LegionControl.Desktop/Contract/AgentCapabilities.cs', `public static class AgentContract
{
    /// The agent version this build carries a bundle of, when it carries one.
    public const string BundledAgentVersion = "${agent}";
}
`);
  write('android/app/src/main/java/com/x1f4r/legioncontrol/agent/Contract.kt', `object AgentContract {
    const val BUNDLED_AGENT_VERSION = "${agent}"
}
`);
  write('android/app/build.gradle.kts', `android {
    defaultConfig {
        applicationId = "com.x1f4r.legioncontrol"
        versionCode = ${build}
        versionName = "${app}"
    }
}

val copyAgentBundle by tasks.registering(Sync::class) {
    val dist = rootProject.layout.projectDirectory.dir("../dist")
    val version = providers.gradleProperty("legion.agentVersion").getOrElse("${agent}")
    from(dist) {
        include("legionctl-agent-$version.tgz")
    }
}
`);
  write('mac/Resources/Info.plist', `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>com.x1f4r.legion-control</string>
	<key>CFBundleShortVersionString</key>
	<string>${app}</string>
	<key>CFBundleVersion</key>
	<string>${build}</string>
</dict>
</plist>
`);
  write('desktop/LegionControl.Desktop/LegionControl.Desktop.csproj', `<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <AssemblyName>legion-control</AssemblyName>
    <Version>${app}</Version>
  </PropertyGroup>
  <ItemGroup>
    <EmbeddedResource Include="../../dist/legionctl-agent-*.tgz" />
  </ItemGroup>
</Project>
`);
  write('android/build-apk.sh', 'for artifact in "legionctl-agent-$AGENT_VERSION.tgz" Legion-Control-agent-manifest.json Legion-Control-agent-manifest.json.sig; do\n');
  write('desktop/build.sh', 'if [ -f "$out/legionctl-agent-$agent_version.tgz" ]; then\n');
  write('mac/build.sh', 'AGENT_ARCHIVE="$DIST_DIR/legionctl-agent-$AGENT_VERSION.tgz"\n');
  write('scripts/release.sh', 'AGENT_ARCHIVE="$DIST/legionctl-agent-$AGENT_VERSION.tgz"\n');
  write('scripts/sign-release.mjs', 'const names = [`legionctl-agent-${agentVersion}.tgz`];\n');
  return root;
}

/** Replaces the first `from` in a file of the tree, and refuses when there is none to replace. */
export function edit(root, file, from, to) {
  const text = fs.readFileSync(path.join(root, file), 'utf8');
  if (!text.includes(from)) throw new Error(`${file} has no ${JSON.stringify(from)} to replace`);
  fs.writeFileSync(path.join(root, file), text.replace(from, to));
}

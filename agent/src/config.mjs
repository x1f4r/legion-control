// The agent's environment and configuration, as one module, for whatever asks
// for them by this name: the release tooling reads AGENT_VERSION from here.
//
// This file used to hold all of it, nearly two thousand lines of paths,
// schema, file handling and process runners. Each part now lives on its own,
// and the agent's modules import from the one they need:
//
//   paths.mjs           which OS this is, where the agent and its files live,
//                       and which version the tree it was loaded from is
//   config-schema.mjs   what config.json may say, and normalizeConfig
//   config-store.mjs    reading and writing config.json, and what a missing
//                       or broken one means
//
// The process runners went to process-runner.mjs, beside the runner every
// mutation goes through.

export * from './paths.mjs';
export * from './config-schema.mjs';
export * from './config-store.mjs';

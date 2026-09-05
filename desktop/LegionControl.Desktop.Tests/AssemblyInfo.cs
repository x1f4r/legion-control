using Xunit;

// The state directory is chosen from an environment variable, which is process-wide. Tests that
// relocate it would otherwise leak into whatever ran beside them, and the failure would look like a
// bug in the code rather than in the harness. They are cheap enough to run one at a time.
[assembly: CollectionBehavior(DisableTestParallelization = true)]

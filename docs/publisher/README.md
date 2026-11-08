# Publishers

[Documentation](../README.md) · [Concepts](../concepts.md) ·
[CLI reference](../reference/README.md)

A publisher signs the catalog that authorizes packages for a repository.
A small local repository can publish reviewed prototypes directly. A
distribution repository can require reproducible builds, independently
verified workers, and checked image compositions.

1. Provision the publisher signing key, review build inputs, and configure
   admission requirements before the first publication.
   [Publish and retire packages](publishing.md)
2. Specify and check release profiles when you ship a coordinated base image.
   [Define signed release profiles](profiles.md)
3. Keep the inputs and attestations needed for independent rebuilds.
   [Archive and verify build evidence](rebuilds.md)
4. Export, sign, and serve a mirror. Provide its trust anchors to clients
   through a trusted channel. [Serve a mirror](mirrors.md)
5. For staged distribution releases, rehearse the candidate before promotion
   and advance rollout after checking the deployed cohort.
   [Rehearse and promote releases](release-process.md)

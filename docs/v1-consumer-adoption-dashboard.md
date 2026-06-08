# v1 Consumer Adoption Dashboard

This dashboard mirrors the cross-consumer tracker in
[#242](https://github.com/zackees/running-process/issues/242) for repo-local
documentation. The consumer tracker issues remain the source of truth; this
document intentionally records no milestone as complete until those trackers
are updated.

## Consumers

| Consumer | Tracker issue | Adoption summary |
|---|---|---|
| soldr-daemon | [zackees/soldr#718](https://github.com/zackees/soldr/issues/718) | Recommended first adopter. soldr already uses prost payloads, so adoption focuses on broker discovery, `BackendHandle`, `.servicedef` packaging, broker-client wiring, and rollout discipline. |
| zccache | [zackees/zccache#698](https://github.com/zackees/zccache/issues/698) | High-traffic consumer. zccache keeps its direct bincode path during transition while adding the prost v1 broker path and coordinated default-on rollout. |
| clud | [zackees/clud#308](https://github.com/zackees/clud/issues/308) | Migrates from the legacy JSON direct path to the prost v1 broker path. Its consumer tracker covers wire migration, broker client integration, `.servicedef`, and rollout work. |
| fbuild | [FastLED/fbuild#510](https://github.com/FastLED/fbuild/issues/510) | External consumer tracker in `FastLED/fbuild`. The first adoption work records the current daemon, wire, cache, and rollback inventory before changing runtime behavior. |

## Milestone Dashboard

Status language matches #242 and stays conservative. Do not mark a cell
complete here unless the corresponding consumer tracker has been updated.

| Consumer | Tracker issue | BackendHandle | .servicedef | Broker client | Default-on | Escape-hatch removal |
|---|---|---|---|---|---|---|
| soldr-daemon | [zackees/soldr#718](https://github.com/zackees/soldr/issues/718) | Open / not started; gated by [#232](https://github.com/zackees/running-process/issues/232) | Open / not started | Open / not started; gated by [#235](https://github.com/zackees/running-process/issues/235) | Blocked on [#238](https://github.com/zackees/running-process/issues/238) | Blocked on [#239](https://github.com/zackees/running-process/issues/239) plus coordinated release wave |
| zccache | [zackees/zccache#698](https://github.com/zackees/zccache/issues/698) | Open / not started; gated by [#232](https://github.com/zackees/running-process/issues/232) | Open / not started | Open / not started; gated by [#235](https://github.com/zackees/running-process/issues/235) | Blocked on [#238](https://github.com/zackees/running-process/issues/238) | Blocked on [#239](https://github.com/zackees/running-process/issues/239) plus coordinated release wave |
| clud | [zackees/clud#308](https://github.com/zackees/clud/issues/308) | Open / not started; gated by [#232](https://github.com/zackees/running-process/issues/232) | Open / not started | Open / not started; gated by [#235](https://github.com/zackees/running-process/issues/235) | Blocked on [#238](https://github.com/zackees/running-process/issues/238) | Blocked on [#239](https://github.com/zackees/running-process/issues/239) plus coordinated release wave |
| fbuild | [FastLED/fbuild#510](https://github.com/FastLED/fbuild/issues/510) | Open / not started; gated by [#232](https://github.com/zackees/running-process/issues/232) | Open / not started | Open / not started; gated by [#235](https://github.com/zackees/running-process/issues/235) | Blocked on [#238](https://github.com/zackees/running-process/issues/238) | Blocked on [#239](https://github.com/zackees/running-process/issues/239) plus coordinated release wave |

## Dependency Gates

| Gate | Milestone | Dashboard rule |
|---|---|---|
| [#232 Phase 2.5 BackendHandle](https://github.com/zackees/running-process/issues/232) | BackendHandle | Consumers stay open until they replace bespoke probe, live-check, and version-match wrappers with `running_process::broker::BackendHandle`. |
| [#235 Phase 4 broker](https://github.com/zackees/running-process/issues/235) | Broker client | Consumers stay open until they wire the broker client helper and its Hello-skip plus broker fallback behavior. |
| [#238 Phase 7 rollout](https://github.com/zackees/running-process/issues/238) | Default-on | Default-on stays blocked until the phase 7 rollout gates are green for each consumer. |
| [#239 Phase 8 escape-hatch removal](https://github.com/zackees/running-process/issues/239) | Escape-hatch removal | Escape-hatch removal stays blocked until the coordinated phase 8 release wave across all four consumers. |

The `.servicedef` milestone remains open for every consumer until that
consumer's installer or package ships a v1 service definition using the schema
and platform locations documented in [v1 service definition](v1-service-definition.md).

## Related Docs

- [clud consumer adoption guide](consumer-adoption-clud.md)
- [zccache consumer adoption guide](consumer-adoption-zccache.md)
- [soldr consumer adoption guide](consumer-adoption-soldr.md)
- [fbuild consumer adoption guide](consumer-adoption-fbuild.md)
- [v1 rollout policy](v1-rollout-policy.md)
- [v1 escape hatch](v1-escape-hatch.md)

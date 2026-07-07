# Attribution

`bundle/` is a vendored subset (14 files) of the **GA4 sample knowledge
bundle** from Google's Open Knowledge Format (OKF) reference repository:

- Source: https://github.com/GoogleCloudPlatform/knowledge-catalog
  (`okf/bundles/ga4/`, commit `d44368c15e38e7c92481c5992e4f9b5b421a801d`,
  fetched 2026-07-07)
- Copyright: Google LLC and contributors
- License: Apache License 2.0 (both the repository root `LICENSE.md` and
  `okf/LICENSE.md` upstream)

Files are unmodified. The subset was chosen to exercise OKF v0.1 consumption:
reserved `index.md` files at several levels, nested directories, relative
cross-links (`../references/metrics/*.md`, same-dir `x.md`), and links to
concepts intentionally NOT vendored (`avg_transactions_per_purchaser.md`,
`avg_spend_per_purchase_session_by_user.md`,
`overall_avg_spend_per_purchase_session.md`) — broken-link tolerance per
OKF §5.3/§9.

This file lives OUTSIDE `bundle/` and is not part of the fixture vault
(tests copy only `bundle/`).

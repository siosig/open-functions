# cf-rs

## 目次

- [概要](#概要)
- [クイックスタート](#クイックスタート)
- [インストール](#インストール)
- [設定](#設定)
- [URL 体系](#url-体系)
- [ps-rs との連携](#ps-rs-との連携)
- [トラブルシューティング](#トラブルシューティング)

## 概要

`cf-rs` は Google Cloud Run functions（Cloud Functions 2nd gen）互換の関数実行環境を、クラウド接続なしにローカル・オンプレで動かす Rust 製サービスです。Rust で書いた関数を Cloud Run functions 向けの Functions Framework 契約でホストし、同じ関数コードを cf-rs 上とそのまま Cloud Run にデプロイして使えます。

対象の詳細は [`specs/001-cloud-functions-local/spec.md`](specs/001-cloud-functions-local/spec.md)、設計は [`plan.md`](specs/001-cloud-functions-local/plan.md) を参照してください。

## クイックスタート

```bash
cargo run -p cf-rs -- serve --data-dir ./tmp/data
cargo run -p cf-rs -- fn deploy hello --source ./examples/hello-http --entry-point hello
curl http://127.0.0.1:8080/hello/world
```

詳細な手順は [`specs/001-cloud-functions-local/quickstart.md`](specs/001-cloud-functions-local/quickstart.md) を参照してください。関数の書き方・SDK の使い方は [`crates/cf-rs-sdk/README.md`](crates/cf-rs-sdk/README.md) を参照してください。

## インストール

3 通りの配備方法がある。いずれも `cf-rs serve` が invoke（既定 `:8080`）と admin（既定 `:8081`）の 2 リスナーを起動する点は共通。

### systemd（バイナリ配布）

GitHub Releases から対象アーキテクチャ（`x86_64` / `aarch64`、musl 静的リンクなので実行に glibc は不要）のアーカイブと `SHA256SUMS` を取得して検証する。

```bash
curl -LO https://github.com/<org>/cf-rs/releases/download/<ver>/cf-rs-<ver>-<target>.tar.gz
curl -LO https://github.com/<org>/cf-rs/releases/download/<ver>/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf cf-rs-<ver>-<target>.tar.gz
sudo install -m 0755 cf-rs /usr/local/bin/cf-rs
```

systemd ユニットを自前で用意する場合は `Type=notify`・`Delegate=yes`（cgroup v2 のメモリ上限を使うため）を設定する。`./ansible` のロールが単一の systemd ユニット＋設定ファイルを配備するので、手動でユニットファイルを書くよりこちらを使うほうが確実（下記）。

### Docker

```bash
docker network create cf-rs   # 初回のみ。関数コンテナと同一ネットワークに置く
docker run -d --name cf-rs \
  -p 8080:8080 -p 8081:8081 \
  -v cf-rs-data:/data \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --group-add "$(getent group docker | cut -d: -f3)" \
  --network cf-rs \
  ghcr.io/<org>/cf-rs:<ver>
```

イメージ方式・コンテナビルドで関数コンテナを起動するには Docker ソケットのマウントと、そのソケットの GID を `--group-add` で明示的に付与する必要がある（`nonroot` ベースイメージのため、ソケットにアクセスできる GID がなければ権限エラーになる）。ソース方式のみで良ければソケットのマウントは不要。

### Ansible（推奨・冪等）

```bash
cd ansible
ansible-galaxy collection install -r requirements.yml
ansible-playbook -i inventory/hosts.yml site.yml -e cf_rs_deploy_mode=systemd   # または docker
```

`cf_rs_deploy_mode`（`systemd` | `docker`）・`cf_rs_build_mode`（`auto` | `host` | `container`）などの変数は [`ansible/README.md`](ansible/README.md) と [`specs/001-cloud-functions-local/contracts/ansible-vars.md`](specs/001-cloud-functions-local/contracts/ansible-vars.md) を参照。2 回目以降の実行は `changed=0` になる（冪等）。ps-rs と同一ホストに同居させる例は `ansible/inventory/hosts.example.yml` にある。

## 設定

設定ファイル（TOML）・環境変数（`CF_RS__*`）・CLI フラグの対応は [`specs/001-cloud-functions-local/contracts/ops-config.md`](specs/001-cloud-functions-local/contracts/ops-config.md) を参照してください。

## URL 体系

呼び出しリスナー（既定 `:8080`）は次の 2 方式で関数名を解決する。ホスト名方式が一致した場合はパス方式より優先する。

| 方式 | 例 | 動作 |
|---|---|---|
| パスプレフィックス | `/hello/world` | 関数 `hello` へ `/world` を転送 |
| ホスト名（`invoke.host_suffix` 設定時） | `Host: hello.fn.local` | 関数 `hello` へパスをそのまま転送 |
| Pub/Sub Push | `POST /_cf/push/hello` | ps-rs からの Push 配信を CloudEvent に変換して転送（`_cf` 以外に予約なし） |

管理リスナー（既定 `:8081`）は `/v1/functions/*`（登録・一覧・削除・ログ）と `/healthz` `/readyz` `/metrics` を提供する。全体は [`specs/001-cloud-functions-local/contracts/admin-api.md`](specs/001-cloud-functions-local/contracts/admin-api.md) を参照。

## ps-rs との連携

Pub/Sub トリガー関数は、姉妹プロジェクト [ps-rs](../ps-rs)（ローカルホスト可能な Pub/Sub 互換サービス）の Push 配信を受けて動きます。連携の詳細は [`contracts/function-contract.md`](specs/001-cloud-functions-local/contracts/function-contract.md) を参照してください。

## トラブルシューティング

- **cgroup メモリ上限の警告が出る**: cgroup v2 が書込不可の環境（Docker 内で `Delegate=` 相当の権限がない、systemd ユニットに `Delegate=yes` を設定していない）では意図的に無効化される。起動は継続し、`memory_mib` の上限のみ効かなくなる。
- **Docker ソケット権限エラー**: イメージ方式・コンテナビルドを使うには実行ユーザー（または Docker コンテナとして動かす場合は cf-rs コンテナ自身）が Docker ソケットにアクセスできる必要がある。Ansible の docker デプロイ方式は対象ホストの `docker` グループ GID を自動で `group_add` するが、手動運用では上記インストール手順の `--group-add` を忘れないこと。
- **glibc 世代の不一致**: ソース方式（ホストビルド）の成果物はビルドしたホスト固有で、glibc 動的リンクのため異なる glibc 世代のマシンへそのまま移すと起動に失敗する。コンテナビルド（`build.mode = container`）はビルド用イメージ `rust:1-bookworm` と実行時イメージ `distroless/cc-debian12` の glibc 世代を意図的に揃えているため、この問題が起きない。複数ホストで同じ成果物を使い回したい場合はコンテナビルドかイメージ方式を使う。

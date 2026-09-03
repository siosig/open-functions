# open-functions

[English](README.md)

## 目次

- [概要](#概要)
- [クイックスタート](#クイックスタート)
- [関数の書き方](#関数の書き方)
- [Python 3.14 関数](#python-314-関数)
- [インストール](#インストール)
- [設定](#設定)
- [URL 体系](#url-体系)
- [関数の管理](#関数の管理)
- [Pub/Sub 連携](#pubsub-連携)
- [観測性](#観測性)
- [トラブルシューティング](#トラブルシューティング)
- [ライセンス](#ライセンス)

## 概要

`open-functions` は Google Cloud Run functions（Cloud Functions 2nd gen）互換の関数実行環境を、クラウド接続なしにローカル・オンプレで動かす Rust 製サービスです。Rust で書いた関数を Cloud Run functions 向けの Functions Framework 契約でホストするため、同じ関数コードを open-functions 上とそのまま Cloud Run にデプロイして使えます。

ワークスペースは 3 つの crate で構成されます。

| Crate | 役割 |
|---|---|
| `open-functions` | ホスト本体（invoke/admin の 2 リスナー、`open-functions fn ...` CLI） |
| `open-functions-core` | ドメイン層（registry・build・runtime・pool・forwarding・Pub/Sub 連携） |
| `open-functions-sdk` | 関数作者が使う SDK |

## クイックスタート

```bash
cargo run -p open-functions -- serve --data-dir ./tmp/data
cargo run -p open-functions -- fn deploy hello --source ./examples/hello-http --entry-point hello
curl http://127.0.0.1:8080/hello/world
```

1 行目でホストを起動（`:8080` が呼び出し用、`:8081` が管理 API 用の 2 リスナー）。2 行目で `examples/hello-http` を実ビルド（実際の `cargo build --release`）し `hello` として登録。`curl` は呼び出しリスナーのパスプレフィックス方式で稼働中のインスタンスに到達する。

## 関数の書き方

HTTP 関数・CloudEvent（Pub/Sub）関数の完全なサンプルコード、構造化ログ、同じソースを実際の Cloud Run functions へ無改変でデプロイする手順は [`crates/open-functions-sdk/README.md`](crates/open-functions-sdk/README.md) を参照してください。

## Python 3.14 関数

Rust に加えて、open-functions は Python 3.14 関数のビルド・実行にも対応しています。書き方は公式の [`functions-framework`](https://pypi.org/project/functions-framework/)（Cloud Run functions 自身が使うものと同じ。open-functions 独自の SDK は不要）に準拠します。

```python
import functions_framework

@functions_framework.http
def hello(request):
    return "Hello from Python!"
```

```bash
open-functions fn deploy hello-py --source ./examples/hello-python-http \
  --entry-point hello --runtime python314
```

`--runtime python314`（あるいは `--source` 配下の `requirements.txt` / `pyproject.toml` からの自動判別）で Rust ではなく Python のビルドパイプラインが選ばれ、`open-functions fn deploy` は依存関係を `uv`（優先）または `pip` で解決します。Rust 関数における `cargo build --release` と同じ位置づけです。

**`python.mode`**（`auto` | `host` | `container`、既定 `auto`）が依存解決をどこで行うかを決めます。

| モード | 動作 |
|---|---|
| `auto` | ホストの `python3.14` を優先し、見つからなければコンテナビルドにフォールバック。どちらも無ければ `412 FAILED_PRECONDITION` |
| `host` | 常にホストのインタプリタでビルド。見つからなければ 412 |
| `container` | 常に `python.container_image` を Docker ソケット経由でビルド。Docker に到達できなければ 412 |

`python.python_bin` でインタプリタの自動判別を上書きできます（空文字なら `python3.14` → `python3` → `python` の順で `PATH` を探索し、最初に見つかった 3.14.x 系を採用）。`python.installer`（`auto` | `uv` | `pip`、既定 `auto`）も同様の考え方で、`auto` は `uv` が使えれば優先し、使えなければインタプリタ自身の `venv` + `pip` にフォールバックします。配布 Docker イメージは常に `python.mode = container` に解決されます（`docker/config.toml` 参照）——distroless の実行イメージ自体には Python も uv も入っていないためです。

### トラブルシューティング

- **Python 関数の登録が `412 FAILED_PRECONDITION` になる**: `host`/`auto` では利用可能な Python 3.14 インタプリタが、`container`/`auto` では到達可能な Docker デーモンが見つからなかったということです。エラー本文の `details.needed` にどちらが必要かが列挙されます。Python 3.14（または `uv`）をホストに入れるか、`python.mode` を実際に使える方式に合わせてください。
- **`PIP_INDEX_URL` などの `PIP_*` 変数が効かない**: `uv` は pip 用の設定（`pip.conf`・`PIP_*`）を意図的に読みません。open-functions 側の制約ではなく uv 自身の公式な設計判断です。`installer` が `uv` に解決される場合（`uv` が使える環境での `auto` の既定挙動）は、代わりに `uv` 自身の変数——`UV_DEFAULT_INDEX`・`UV_INDEX`・`UV_EXTRA_INDEX_URL`——と、`uv`・`pip` 双方が読む標準変数（`HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`/`SSL_CERT_FILE`/`SSL_CERT_DIR`/`NETRC`）を使ってください。これらは open-functions 自身のプロセス環境（systemd ユニットの `Environment=`/`EnvironmentFile=`、または Docker コンテナの環境変数——Ansible 管理下では `ansible/` の `open_functions_extra_env` に相当）に設定するものであり、関数自身の実行時環境には渡されません。
- **ネイティブ拡張を含む依存関係のビルドに失敗する**: host 方式の依存解決には C ツールチェインが必要ですが、container 方式の既定イメージ（`python.container_image` = `ghcr.io/astral-sh/uv:python3.14-trixie-slim`）にもコンパイラは入っていません。`cp314`/`manylinux` 向けの wheel を配布している依存パッケージを優先するか、必要なコンパイラツールチェインを含むカスタムイメージを `python.container_image` に指定してください。

## インストール

3 通りの配備方法がある。いずれも `open-functions serve` が invoke（既定 `:8080`）と admin（既定 `:8081`）の 2 リスナーを起動する点は共通。

### systemd（バイナリ配布）

GitHub Releases から対象アーキテクチャ（`x86_64` / `aarch64`、musl 静的リンクなので実行に glibc は不要）のアーカイブと `SHA256SUMS` を取得して検証する。

```bash
curl -LO https://github.com/siosig/open-functions/releases/download/<ver>/open-functions-<ver>-<target>.tar.gz
curl -LO https://github.com/siosig/open-functions/releases/download/<ver>/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
tar -xzf open-functions-<ver>-<target>.tar.gz
sudo install -m 0755 open-functions /usr/local/bin/open-functions
```

systemd ユニットを自前で用意する場合は `Type=notify`・`Delegate=yes`（cgroup v2 のメモリ上限を使うため）を設定する。`./ansible` のロールが単一の systemd ユニット＋設定ファイルを配備するので、手動でユニットファイルを書くよりこちらを使うほうが確実（下記）。

### Docker

```bash
docker network create open-functions   # 初回のみ。関数コンテナと同一ネットワークに置く
docker run -d --name open-functions \
  -p 8080:8080 -p 8081:8081 \
  -v open-functions-data:/data \
  -v /var/run/docker.sock:/var/run/docker.sock \
  --group-add "$(getent group docker | cut -d: -f3)" \
  --network open-functions \
  ghcr.io/siosig/open-functions:<ver>
```

イメージ方式・コンテナビルドで関数コンテナを起動するには Docker ソケットのマウントと、そのソケットの GID を `--group-add` で明示的に付与する必要がある（`nonroot` ベースイメージのため、ソケットにアクセスできる GID がなければ権限エラーになる）。ソース方式のみで良ければソケットのマウントは不要。

### Ansible（推奨・冪等）

```bash
cd ansible
ansible-galaxy collection install -r requirements.yml
ansible-playbook -i inventory/hosts.yml site.yml -e open_functions_deploy_mode=systemd   # または docker
```

`open_functions_deploy_mode`（`systemd` | `docker`）・`open_functions_build_mode`（`auto` | `host` | `container`）などの変数一覧は [`ansible/README.md`](ansible/README.md) を参照。2 回目以降の実行は `changed=0` になる（冪等）。同居させるサービスの例は `ansible/inventory/hosts.example.yml` にある。

## 設定

設定は既定値 → TOML 設定ファイル → 環境変数（`OPEN_FUNCTIONS__*`、セクション区切りは `__`。例: `OPEN_FUNCTIONS__ADMIN__LISTEN=0.0.0.0:8081`）→ CLI フラグの順で上書きされる（優先度は右ほど高い）。未知のキーは起動失敗になる。

主なセクション: `[invoke]` / `[admin]`（bind アドレス・host suffix・admin token）、`[storage]`（`data_dir`）、`[build]`（`mode`: `auto` | `host` | `container`、`cargo_bin`、`timeout_secs`）、`[runtime]`（`docker_socket`・`cgroup`・`max_total_instances`・`stop_grace_secs`）、`[pubsub]`（`enabled`・`base_url`・`project`）、`[log]`（`format`・`level`・`function_ring_buffer_lines`）、`[metrics]`（`enabled`）、`[defaults]`（関数登録時の既定値: `timeout_secs`・`concurrency`・`memory_mib`・`min_instances`・`max_instances`・`queue_policy` など）。

`open-functions check-config` で起動せずに設定ファイル（と環境変数上書き）の妥当性を検証できる。

## URL 体系

呼び出しリスナー（既定 `:8080`）は次の 2 方式で関数名を解決する。ホスト名方式が一致した場合はパス方式より優先する。

| 方式 | 例 | 動作 |
|---|---|---|
| パスプレフィックス | `/hello/world` | 関数 `hello` へ `/world` を転送 |
| ホスト名（`invoke.host_suffix` 設定時） | `Host: hello.fn.local` | 関数 `hello` へパスをそのまま転送 |
| Pub/Sub Push | `POST /_cf/push/hello` | Push 配信を CloudEvent に変換して転送（`_cf` 以外に予約なし） |

管理リスナー（既定 `:8081`）は `/v1/functions/*`（登録・一覧・詳細・削除・ビルド/関数ログ・停止）と `/healthz` `/readyz` `/metrics` を提供する。`admin.listen` が非 loopback にバインドされている場合、`/v1/*` は `Authorization: Bearer <token>` を要求する。

## 関数の管理

```bash
open-functions fn deploy <name> --source <dir> | --image <ref> [--trigger-http | --trigger-topic <topic>] [--entry-point <fn>] [...]
open-functions fn list
open-functions fn describe <name>
open-functions fn delete <name> [--wait]
open-functions fn logs <name> [--tail <n>] [--follow]
open-functions fn build-log <name> [--build <id>] [--follow]
open-functions fn stop <name>
```

`--source` はローカルディレクトリからビルド（`build.mode`/`python.mode` に応じてホスト `cargo`/`uv`/`pip` またはコンテナ内ビルド）、`--image` は事前ビルド済みのコンテナイメージをそのまま実行する（Docker デーモンへの到達が必須）。`deploy` は既定でビルド完了まで追従する。`--no-wait` で受理された時点ですぐ戻る。出力は TTY なら table、それ以外は JSON（`--output json|table` で強制も可）。どの管理 API と通信するかは `OPEN_FUNCTIONS_ADMIN_URL`（既定 `http://127.0.0.1:8081`）・`OPEN_FUNCTIONS_ADMIN_TOKEN` で設定する。

Python 関数の Dockerfile（[`examples/hello-python-http/Dockerfile`](examples/hello-python-http/Dockerfile) 参照）は、Cloud Run 関数の `--base-image python314` によるコンテナソースデプロイと*同じ*イメージなので、ここでの `--image` と実際の GCP デプロイは同一に動作する——open-functions 固有の細工は一切無い:

```bash
docker build -t hello-python-http:dev examples/hello-python-http
open-functions fn deploy hello-py-img --image hello-python-http:dev \
  --entry-point hello --runtime python314
```

`--runtime` はイメージ方式では表示用のヒントとして保存されるだけ（`fn list` の `RUNTIME` 列、`fn describe` の `runtime` フィールド）——open-functions はイメージの中身を検査もビルドもしないため、イメージ方式の登録は宣言された任意の runtime（省略時は `null`）を検証なしで受理する。

## Pub/Sub 連携

`--trigger-topic` を指定した関数は、Pub/Sub 互換の REST サービス（姉妹プロジェクト `open-pubusb` がクラウド接続なしでローカルに実装）からの Push 配信で起動する。open-functions は各 Push 配信を `google.cloud.pubsub.topic.v1.messagePublished` CloudEvent に変換してから関数へ転送し、関数の応答に応じて ack するか再送に委ねる——実際の Cloud Run 関数に対する Eventarc の Pub/Sub イベント配信と同じ挙動。

Python 関数も同じイベントを公式の `functions_framework.cloud_event` デコレータで受け取る（[`examples/hello-python-pubsub`](examples/hello-python-pubsub) 参照）:

```bash
open-functions fn deploy on-orders-py \
  --source ./examples/hello-python-pubsub \
  --entry-point on_msg \
  --trigger-topic orders-py
```

```python
import base64
import functions_framework

@functions_framework.cloud_event
def on_msg(cloud_event):
    message = cloud_event.data["message"]
    text = base64.b64decode(message["data"]).decode("utf-8")
    print(f"received: {text}")
```

`cloud_event.data` は GCP の `MessagePublishedData` 形式そのもの: `message.data` は base64 エンコードされている（上記のように自前でデコードする）、`message.attributes` は通常の dict、`cloud_event["type"]` / `cloud_event["source"]` は Rust SDK の `CloudEvent` と同じ CloudEvent エンベロープの type/source を持つ——言語が違っても同じ契約を見る。

## 観測性

構造化ログ（`format = "json"`）は `severity`・`time`・`message` を持ち、関数由来の行はさらに `source="function"`・`function`・`revision`・`instance_id`・`execution_id` を持つ。`/metrics` は `open_functions_` 接頭辞の Prometheus メトリクスを公開する: 呼び出し数・処理時間、転送オーバーヘッド、関数別インスタンス数・コールドスタート時間、ビルド結果、Pub/Sub binding 状態など。

## トラブルシューティング

- **cgroup メモリ上限の警告が出る**: cgroup v2 が書込不可の環境（Docker 内で `Delegate=` 相当の権限がない、systemd ユニットに `Delegate=yes` を設定していない）では意図的に無効化される。起動は継続し、`memory_mib` の上限のみ効かなくなる。
- **Docker ソケット権限エラー**: イメージ方式・コンテナビルドを使うには実行ユーザー（または Docker コンテナとして動かす場合は open-functions コンテナ自身）が Docker ソケットにアクセスできる必要がある。Ansible の docker デプロイ方式は対象ホストの `docker` グループ GID を自動で `group_add` するが、手動運用では上記インストール手順の `--group-add` を忘れないこと。
- **glibc 世代の不一致**: ソース方式（ホストビルド）の成果物はビルドしたホスト固有で、glibc 動的リンクのため異なる glibc 世代のマシンへそのまま移すと起動に失敗する。コンテナビルド（`build.mode = container`）はビルド用イメージ `rust:1-bookworm` と実行時イメージ `distroless/cc-debian12` の glibc 世代を意図的に揃えているため、この問題が起きない。複数ホストで同じ成果物を使い回したい場合はコンテナビルドかイメージ方式を使う。

## ライセンス

[Apache License, Version 2.0](LICENSE-APACHE) または [MIT license](LICENSE-MIT) のいずれかを選択できます。

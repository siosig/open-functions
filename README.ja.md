# cf-rs

## 目次

- [概要](#概要)
- [クイックスタート](#クイックスタート)
- [提供形態](#提供形態)
- [設定](#設定)
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

詳細な手順は [`specs/001-cloud-functions-local/quickstart.md`](specs/001-cloud-functions-local/quickstart.md) を参照してください。

## 提供形態

- systemd ユニット（バイナリ配布）
- Docker コンテナ
- Ansible（`./ansible`、`cf_rs_deploy_mode=systemd|docker`）

## 設定

設定ファイル（TOML）・環境変数（`CF_RS__*`）・CLI フラグの対応は [`specs/001-cloud-functions-local/contracts/ops-config.md`](specs/001-cloud-functions-local/contracts/ops-config.md) を参照してください。

## ps-rs との連携

Pub/Sub トリガー関数は、姉妹プロジェクト [ps-rs](../ps-rs)（ローカルホスト可能な Pub/Sub 互換サービス）の Push 配信を受けて動きます。連携の詳細は [`contracts/function-contract.md`](specs/001-cloud-functions-local/contracts/function-contract.md) を参照してください。

## トラブルシューティング

- cgroup メモリ上限の警告が出る: cgroup v2 が書込不可の環境（Docker 内、systemd `Delegate=` 未設定）では意図的に無効化されます。
- Docker ソケット権限エラー: イメージ方式・コンテナビルドを使うには実行ユーザーが Docker ソケットにアクセスできる必要があります。

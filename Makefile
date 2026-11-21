# macOS付属のGNU Make 3.81でも動作する。
.DEFAULT_GOAL := help

CARGO_FLAGS ?= --locked
INSTALL_DIR ?= $(HOME)/Library/Input Methods
ifneq ($(origin INSTALL_PATH),undefined)
INSTALL_DIR := $(INSTALL_PATH)
endif
INSTALL_PATH ?= $(INSTALL_DIR)
INSTALL_DICTIONARY ?= 1
ENABLE_INPUT_SOURCE ?= 0
KANADE_DATA_DIR ?= $(HOME)/Library/Application Support/kanade-ime
MISE_CANDIDATES ?= $(HOME)/.local/bin/mise /opt/homebrew/bin/mise /usr/local/bin/mise
SETUP_TOOLS ?= rust bazelisk python cargo:owayo/astro-sight

ifeq ($(SYSTEM_TOOLS),1)
RUN :=
else
ifndef MISE
MISE := $(firstword $(shell command -v mise 2>/dev/null) $(wildcard $(MISE_CANDIDATES)))
endif
ifeq ($(MISE),)
ifneq ($(filter-out help,$(or $(MAKECMDGOALS),help)),)
$(error miseが見つかりません。https://mise.jdx.dev から導入するか、SYSTEM_TOOLS=1を指定してください)
endif
endif
RUN := $(if $(MISE),"$(MISE)" exec --,)
endif

XTASK = $(RUN) cargo run $(CARGO_FLAGS) --package xtask --
APP = $(INSTALL_DIR)/Kanade.app

# 既存の開発用証明書を使う。呼び出し側の証明書名・ハッシュ指定を優先する。
# 認証付きIPCには証明書が必要なため、ad-hoc署名へは切り替えない。
CODESIGN_IDENTITY ?= $(shell security find-identity -v -p codesigning 2>/dev/null | awk '/"[^"]* local dev"$$/ {print $$2; exit}')
export CODESIGN_IDENTITY
export KANADE_DATA_DIR
export KANADE_INSTALL_DIR = $(INSTALL_DIR)

.PHONY: help setup prepare build release worker bundle install verify-sign settings status \
	test lint fmt fmt-check check ci contract learning-contract project-contract evaluate input-regression bench \
	dictionary llm-worker llm-contract depup clean require-macos require-signing version bump-version verify native-check uninstall dev run distribution-check

.PHONY: emoji-data
EMOJI_FLAGS ?=
ifeq ($(filter --offline,$(EMOJI_FLAGS)),)
emoji-data: dictionary ## 固定したソースと辞書から絵文字の読みを生成する
endif
emoji-data: prepare ## 固定したソースと辞書から絵文字の読みを生成する
	$(XTASK) emoji-data --dictionary "$(KANADE_DATA_DIR)/dict/ipadic.hsd" $(EMOJI_FLAGS)

setup: ## ツールチェーン (mise) と依存を取得する
	@if [ -n "$(MISE)" ]; then "$(MISE)" install $(SETUP_TOOLS); fi
	$(MAKE) prepare
	$(RUN) cargo fetch $(CARGO_FLAGS)

prepare: ## 固定したサブモジュールを取得し、未変更であることを検証する
	git submodule update --init --recursive
	$(XTASK) prepare

build: prepare ## デバッグ版をビルドする
	$(RUN) cargo build $(CARGO_FLAGS) --workspace

worker: prepare ## 無改造のMozcワーカーをBazelで明示的にビルドする
	$(XTASK) build-worker

llm-worker: prepare ## CPU用のモデルワーカーをビルドする (CMakeとlibclangが必要)
	$(RUN) cargo build $(CARGO_FLAGS) -p kanade-llm --features llama --bin kanade-llm-worker --release

llm-contract: worker ## 手動導入したモデルを検証する (KANADE_LLM_MODELに絶対パスを指定)
	@test -n "$(KANADE_LLM_MODEL)" || { echo 'Set KANADE_LLM_MODEL to the supported GGUF file. python3 .github/scripts/bundled_model.py --fetch prints the path of the bundled one.' >&2; exit 1; }
	KANADE_LLM_MODEL="$(KANADE_LLM_MODEL)" $(RUN) cargo test $(CARGO_FLAGS) -p kanade-llm --features llama --test real_model -- --ignored --nocapture --test-threads 1
	KANADE_LLM_MODEL="$(KANADE_LLM_MODEL)" $(RUN) cargo test $(CARGO_FLAGS) -p kanade-llm --features llama,fixture-profiles --test real_model -- --ignored --nocapture --test-threads 1
	$(RUN) cargo build $(CARGO_FLAGS) -p kanade-llm --features llama --bin kanade-llm-worker
	KANADE_LLM_MODEL="$(KANADE_LLM_MODEL)" KANADE_LLM_WORKER="$(CURDIR)/target/debug/kanade-llm-worker" KANADE_ALLOW_UNSIGNED=1 $(RUN) cargo test $(CARGO_FLAGS) -p kanade-engine --test llm_socket -- --ignored --test-threads 1

require-macos: ## macOSであることを確かめる
	@test "$$(uname -s)" = Darwin || { echo "Local IME installation is currently supported on macOS only." >&2; exit 1; }

require-signing: require-macos ## コード署名用の証明書を確認する
	@test -n "$(CODESIGN_IDENTITY)" && test "$(CODESIGN_IDENTITY)" != - || { \
		echo 'A code-signing certificate is required for authenticated IPC.' >&2; \
		echo 'Set CODESIGN_IDENTITY to an existing certificate name or SHA-1 hash.' >&2; exit 1; }

# ビルド前に署名条件を確認する。
bundle: require-signing release ## リリース版をアプリにまとめる
	$(MAKE) worker
	$(XTASK) bundle-macos

release: prepare ## リリース版をビルドする
	$(RUN) cargo build --release $(CARGO_FLAGS) --workspace --features kanade-ui/native,kanade-llm/llama

dev: prepare ## デバッグ版のアプリを起動する
	$(RUN) cargo run $(CARGO_FLAGS) -p kanade-ui --features native

run: install ## リリース版のアプリをインストールして起動する
	$(MAKE) settings

version: ## 共通のバージョンを検証して表示する
	$(XTASK) version

VERSION_FLAGS ?=
bump-version: ## 日本時間で採番する (VERSION_FLAGS=--dry-runでプレビュー)
	$(XTASK) bump-version $(VERSION_FLAGS)

install: require-signing bundle ## リリース版のアプリを INSTALL_DIR に入れる
	@if [ "$(INSTALL_DICTIONARY)" = 1 ]; then $(XTASK) fetch-dictionary; fi
	KANADE_INSTALL_DIR="$(INSTALL_DIR)" $(XTASK) install-macos $(if $(filter 1,$(ENABLE_INPUT_SOURCE)),--enable,)

uninstall: require-macos ## INSTALL_DIR からアプリを取り除く
	KANADE_INSTALL_DIR="$(INSTALL_DIR)" $(XTASK) install-macos --uninstall

status: require-macos ## 入力ソースの検出状態を確認する (登録や有効化はしない)
	$(XTASK) install-macos --status

verify-sign: require-macos ## インストール済みアプリの署名を検証して表示する
	codesign --verify --deep --strict "$(APP)"
	codesign -d --verbose=2 "$(APP)"

settings: require-macos ## インストール済みの設定アプリを開く
	open "$(APP)/Contents/Helpers/Settings.app"

test: prepare ## テストを実行する
	$(RUN) cargo test $(CARGO_FLAGS) --workspace

lint: prepare ## clippy を警告ゼロで通す
	$(RUN) cargo clippy $(CARGO_FLAGS) --workspace --all-targets --all-features -- -D warnings

fmt: ## コードを整形する (書き換える)
	$(RUN) cargo fmt --all

fmt-check: ## 整形済みかを確かめる (書き換えない)
	$(RUN) cargo fmt --all -- --check

check: fmt-check lint verify ## 整形と静的検査 (書き換えない)

ci: check test native-check distribution-check macos-installer-check ## CI と同じ検査 (書き換えない)

.PHONY: macos-installer-check
macos-installer-check: ## macOSインストーラーを検証する
	@if [ "$$(uname -s)" = Darwin ]; then swift test --package-path packaging/macos/installer --scratch-path artifacts/swift-installer -Xswiftc -warnings-as-errors; fi

distribution-check: ## 配布アーカイブの欠落・混入・改ざんの拒否を検証する
	$(RUN) python -m unittest discover -s .github/scripts -p 'test_*.py'

verify: prepare ## バージョン、生成済みデータとMozcの固定を検証する
	$(XTASK) version
	$(XTASK) verify

native-check: ## macOSの設定画面を検証する
	@if [ "$$(uname -s)" = Darwin ]; then $(RUN) cargo check $(CARGO_FLAGS) -p kanade-ui --features native; fi

contract: export RUST_TEST_THREADS = 1 ## 実ワーカーの検証を直列で実行する
contract: worker ## 実ワーカーの変換・反映・復旧を検証する
	$(RUN) cargo run $(CARGO_FLAGS) -p kanade-eval -- contract
	$(RUN) cargo test $(CARGO_FLAGS) -p kanade-eval -- --ignored
	$(RUN) cargo test $(CARGO_FLAGS) -p kanade-mozc --test learning --test symbols -- --ignored
	$(RUN) cargo test $(CARGO_FLAGS) -p kanade-mozc --lib -- --ignored
	$(RUN) cargo test $(CARGO_FLAGS) -p kanade-engine --test contracts --test selection_learning -- --ignored --skip dictionary_
	$(RUN) cargo test $(CARGO_FLAGS) -p kanade-engine --test socket -- --ignored
	$(RUN) cargo test $(CARGO_FLAGS) -p kanade-engine --test dictionary -- --ignored --skip project_approval_and_rebuild

learning-contract: worker dictionary ## 辞書を使う訂正学習を検証する
	KANADE_TEST_DICT="$(KANADE_DATA_DIR)/dict/ipadic.hsd" $(RUN) cargo test $(CARGO_FLAGS) -p kanade-engine --test contracts dictionary_ -- --ignored

evaluate: worker ## 変換の基本的な評価を実行する
	$(RUN) cargo run $(CARGO_FLAGS) -p kanade-eval -- evaluate

input-regression: worker ## 広範な合成入力で変換精度を評価する
	$(RUN) cargo run $(CARGO_FLAGS) -p kanade-eval -- evaluate eval/input-regression.jsonl

project-contract: worker ## 固定した解析器でプロジェクト索引・監視・エンジン連携を検証する
	$(RUN) sh -c 'KANADE_ASTRO_SIGHT="$${KANADE_ASTRO_SIGHT:-$$(command -v astro-sight)}" exec cargo test $(CARGO_FLAGS) -p kanade-indexer --lib --test indexing --test watching -- --include-ignored --test-threads 1'

	$(RUN) sh -c 'KANADE_ASTRO_SIGHT="$${KANADE_ASTRO_SIGHT:-$$(command -v astro-sight)}" exec cargo test $(CARGO_FLAGS) -p kanade-engine --test dictionary project_approval_and_rebuild -- --ignored --test-threads 1'

bench: worker ## 起動済みワーカーへの通信遅延を測定する
	$(RUN) cargo run $(CARGO_FLAGS) -p kanade-eval -- bench

dictionary: prepare ## 固定した学習辞書とライセンス表示を取得・検証する
	$(XTASK) fetch-dictionary

depup: ## 固定したdepupのタスクで依存を更新する
ifeq ($(SYSTEM_TOOLS),1)
	depup --rust --mise --exclude hasami --exclude noslop --exclude gpui --exclude gpui_platform --exclude windows --exclude windows-core --exclude llama-cpp-sys-2 --install
else
	"$(MISE)" run depup
endif

clean: ## ビルド成果物を消す
	$(RUN) cargo clean

help: ## このヘルプを表示する
	@printf 'Kanade IMEの開発タスク\n\n使い方: make <ターゲット>\n\n'
	@awk 'BEGIN {FS = ":.*?## "} /^[a-zA-Z0-9_-]+:.*?## / {printf "  %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@printf '\nmake installはmiseを使い、既定では~/Library/Input Methodsへ導入します。\n'
	@printf '上書き指定: INSTALL_DIR, INSTALL_PATH, CODESIGN_IDENTITY, INSTALL_DICTIONARY=0, ENABLE_INPUT_SOURCE=1, SYSTEM_TOOLS=1\n'
	@printf 'ツールの版は mise.toml で固定しています。\nGitHub Actions > Release > Run workflow で配布物を作成します。\n'

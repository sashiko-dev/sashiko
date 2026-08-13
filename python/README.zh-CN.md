# GPU Driver Code Scan

[简体中文](README.zh-CN.md) | [English](README.md)

## 功能简介

`gpu-driver-code-scan` 是一个面向 Linux GPU 驱动源码目录的本地扫描程序。它会建立完整源码清单，按风险对代码分组并调用 [Sashiko](https://github.com/FFizzZZ/sashiko) 审查，最后生成版本化 JSON 产物和 Markdown 报告。

它既可以安装为标准 Python CLI，也可以安装为一个轻量 Agent Skill。两种方式都会运行同一套扫描实现，并生成相同的版本化产物。

## 扫描策略

![GPU 驱动扫描策略](docs/scan-strategy.svg)

1. **建立清单**：枚举可审查源码文件，排除生成目录和测试输出。
2. **代码分组**：优先按目录组织；超大文件按连续行区间切分，并同时满足文件数、代码行数和字节数限制。
3. **风险排序**：优先扫描 UVM/HMM、缺页处理、DMA/IOMMU、用户态接口、计算队列、锁和对象生命周期等高风险路径。
4. 对完整源码树 `T` 中的每个分组 `G` 构造一组 Synthetic Diff：

   ```text
   Baseline = T - G
   Target   = T
   ```

   Diff 只包含当前分组 `G`，但 Target Commit 仍保留完整源码树，因此 Sashiko 能读取全局上下文，并保持原始文件行号。
5. **并发审查和产物生成**：并发运行各分组，归一化 findings，校验所有源码区间均已分配，最后写出 `scan-result.json`、`findings.json` 和 `report.md`。

`--max-findings` 是停止提交新分组的阈值，不是报告条数上限。已经开始的分组会继续完成，其发现都会保留在最终报告中。

## 命令行

首次初始化并安装：

```bash
git clone https://github.com/FFizzZZ/sashiko.git
cd sashiko
cargo build --release --bin review --manifest-path Cargo.toml
cd python
python3 -m pip install -e .
```

也可以把同一个程序安装为 Agent Skill：

```bash
scripts/install-skill --target ~/.codex
```

安装后的 `$gpu-driver-code-scan` Skill 会收集本地源码目录和输出目录，然后通过 CLI 调用同一套 Scanner 实现。

按公共默认参数执行：

```bash
gpu-driver-code-scan scan /path/to/driver \
  --output-dir ./artifacts/run-001
```

| 参数 | 说明 | 默认值 |
|---|---|---|
| `source_dir` | 已存在的本地驱动源码目录 | 必填 |
| `--output-dir` | 用于生成扫描产物的空目录 | 必填 |
| `--project` | 产物和报告中的项目名 | 源码目录名 |
| `--driver-url` | 报告中展示的可选驱动定位信息 | 空 |
| `--kernel-url` | 报告中展示的可选内核上下文定位信息 | 空 |
| `--provider` | Sashiko AI Provider | `codex-cli` |
| `--model` | Provider 使用的模型 | `gpt-5.5-2026-04-24` |
| `--concurrency` | 并发扫描分组数 | `3` |
| `--max-findings` | 停止提交新分组的 finding 阈值 | `10` |
| `--max-files-per-group` | 单个分组的最大文件数 | `30` |
| `--max-lines-per-group` | 单个分组的最大目标代码行数 | `1000` |
| `--max-bytes-per-group` | 单个分组的最大目标字节数 | `100000` |
| `--max-review-seconds` | 整次扫描审查预算；`0` 表示不限制 | `7200` |
| `--review-timeout-seconds` | 单个 Sashiko 分组的超时时间 | `3600` |
| `--stages` | Sashiko Review Stages | `3,4,5,6,7` |
| `--include` | 额外加入清单的 glob，可重复传入 | 无 |
| `--plan-only` | 只生成清单、分组和 Patch Map，不调用模型 | `false` |
| `--no-ai` | 不调用模型，仅验证 Sashiko Patch 处理链路 | `false` |

校验已完成的输出目录：

```bash
gpu-driver-code-scan validate ./artifacts/run-001
```

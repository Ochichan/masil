#!/usr/bin/env python3
"""Run repeated single-product benchmarks in alternating product order."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from typing import Any

from benchmark import geometry_comparability


ROOT = Path(__file__).resolve().parents[2]
BENCHMARK = Path(__file__).with_name("benchmark.py")


def parse_products(value: str) -> list[str]:
    products = [item.strip() for item in value.split(",") if item.strip()]
    if len(products) < 2:
        raise argparse.ArgumentTypeError("comparison requires at least two products")
    if len(products) != len(set(products)):
        raise argparse.ArgumentTypeError("products must be unique")
    return products


def alternating_order(products: list[str], round_index: int) -> list[str]:
    return list(products if round_index % 2 == 0 else reversed(products))


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--products", type=parse_products, default=parse_products("masil,herdr"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("benchmark_args", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    if args.rounds < 2:
        parser.error("--rounds must be at least 2 so each product runs first")
    if args.benchmark_args[:1] == ["--"]:
        args.benchmark_args = args.benchmark_args[1:]
    forbidden = ("--products", "--output")
    if any(argument == item or argument.startswith(item + "=") for argument in args.benchmark_args for item in forbidden):
        parser.error("pass --products and --output to compare.py, not to benchmark.py")
    if args.output.exists():
        parser.error(f"refusing to overwrite existing output: {args.output}")
    return args


def assess_runs(runs: list[dict[str, Any]], products: list[str], rounds: int) -> dict[str, Any]:
    errors: list[str] = []
    provenance: dict[str, dict[str, Any]] = {}
    round_geometry: list[dict[str, Any]] = []
    run_context: dict[str, Any] | None = None
    for item in runs:
        report = item.get("report")
        if not report:
            continue
        candidate = {
            "harness": report.get("harness"),
            "methodology": report.get("methodology"),
            "host": report.get("host"),
        }
        if run_context is None:
            run_context = candidate
        elif candidate != run_context:
            errors.append(
                f"round {item['round'] + 1} {item['product']}: "
                "harness hashes, methodology, or host metadata changed between runs"
            )
    for product in products:
        product_runs = [item for item in runs if item["product"] == product]
        if len(product_runs) != rounds:
            errors.append(f"{product}: expected {rounds} runs, found {len(product_runs)}")
            continue
        binary_records = [
            item["report"]["products"][product]["binary"]
            for item in product_runs
            if item.get("report") and product in item["report"].get("products", {})
        ]
        if len(binary_records) != rounds:
            errors.append(f"{product}: one or more runs omitted binary provenance")
        elif any(record != binary_records[0] for record in binary_records[1:]):
            errors.append(f"{product}: binary provenance changed between rounds")
        elif binary_records:
            provenance[product] = binary_records[0]

    for round_index in range(rounds):
        selected = [item for item in runs if item["round"] == round_index]
        by_product: dict[str, Any] = {}
        for item in selected:
            product = item["product"]
            if item["exit_code"] != 0:
                errors.append(f"round {round_index + 1} {product}: benchmark exited {item['exit_code']}")
            report = item.get("report")
            if report and product in report.get("products", {}):
                by_product[product] = report["products"][product]
            else:
                errors.append(f"round {round_index + 1} {product}: benchmark report missing")
        if set(by_product) == set(products):
            geometry = geometry_comparability(by_product)
            round_geometry.append({"round": round_index + 1, **geometry})
            if not geometry["all_conditions_matched"]:
                errors.append(f"round {round_index + 1}: measured inner geometry did not match")

    return {
        "status": "valid" if not errors else "invalid",
        "errors": errors,
        "stable_binary_provenance": provenance,
        "stable_run_context": run_context,
        "geometry_by_round": round_geometry,
        "direct_comparison_supported": not errors,
    }


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    started = datetime.now(timezone.utc)
    runs: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="masil-comparison-") as directory:
        temporary = Path(directory)
        for round_index in range(args.rounds):
            order = alternating_order(args.products, round_index)
            for order_index, product in enumerate(order):
                output = temporary / f"round-{round_index + 1}-{product}.json"
                command = [
                    sys.executable,
                    str(BENCHMARK),
                    *args.benchmark_args,
                    "--products",
                    product,
                    "--output",
                    str(output),
                ]
                print(
                    f"round {round_index + 1}/{args.rounds}, {product} ({order_index + 1}/{len(order)})",
                    file=sys.stderr,
                    flush=True,
                )
                result = subprocess.run(
                    command,
                    cwd=ROOT,
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    check=False,
                )
                report = json.loads(output.read_text()) if output.is_file() else None
                runs.append(
                    {
                        "round": round_index,
                        "order_index": order_index,
                        "product": product,
                        "command": command,
                        "exit_code": result.returncode,
                        "stdout": result.stdout,
                        "stderr": result.stderr,
                        "report": report,
                    }
                )

    assessment = assess_runs(runs, args.products, args.rounds)
    report = {
        "schema_version": 1,
        "kind": "alternating_product_comparison",
        "started_at_utc": started.isoformat(),
        "completed_at_utc": datetime.now(timezone.utc).isoformat(),
        "rounds": args.rounds,
        "base_product_order": args.products,
        "orders": [alternating_order(args.products, index) for index in range(args.rounds)],
        "benchmark_args": args.benchmark_args,
        "scope": (
            "Repeated terminal PTY workloads with alternating product order. "
            "No agent management, provider account, model request, or full-product comparison."
        ),
        "assessment": assessment,
        "runs": runs,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return 0 if assessment["status"] == "valid" else 1


if __name__ == "__main__":
    sys.exit(main())

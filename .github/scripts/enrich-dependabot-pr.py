#!/usr/bin/env python3
"""Enrich Dependabot pull requests with registry metadata and clearer titles."""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass


BUMP_TITLE_RE = re.compile(
    r"(?:bump|update)\s+([^\s]+)(?:\s+requirements?)?\s+from\s+([^\s]+)\s+to\s+([^\s]+)",
    re.IGNORECASE,
)
BUMP_BODY_RE = re.compile(
    r"Bumps\s+\[(?P<name>[^\]]+)\]\([^)]+\)\s+from\s+(?P<old>[^\s]+)\s+to\s+(?P<new>[^\s]+)",
    re.IGNORECASE,
)
GROUP_BODY_RE = re.compile(
    r"Bumps\s+the\s+(?P<group>[^\s]+)\s+group\s+with\s+(?P<count>\d+)\s+updates?:",
    re.IGNORECASE,
)
GROUP_UPDATE_RE = re.compile(
    r"^Updates\s+`(?P<name>[^`]+)`\s+from\s+(?P<old>[^\s]+)\s+to\s+(?P<new>[^\s]+)",
    re.IGNORECASE | re.MULTILINE,
)
MARKER = "<!-- razor-dependabot-enriched -->"
ORIGINAL_SUMMARY = "<summary>Original Dependabot description</summary>"
VERSION_PREFIX_RE = re.compile(r"^[<>=!~,\s]+")


@dataclass(frozen=True)
class Bump:
    name: str
    old_version: str
    new_version: str
    updates: tuple[Bump, ...] = ()
    group: str | None = None


@dataclass(frozen=True)
class RegistryInfo:
    summary: str
    homepage: str | None
    repository: str | None
    changelog: str | None
    documentation: str | None
    license: str | None
    published: str | None


def _fetch_json(url: str) -> dict | None:
    request = urllib.request.Request(
        url,
        headers={"Accept": "application/json", "User-Agent": "razor-dependabot-enrich/1.0"},
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return json.load(response)
    except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, json.JSONDecodeError):
        return None


def _first_url(project_urls: dict[str, str] | None, *keys: str) -> str | None:
    if not project_urls:
        return None
    lowered = {key.lower(): value for key, value in project_urls.items()}
    for key in keys:
        value = lowered.get(key.lower())
        if value:
            return value
    return None


def _clean_token(value: str) -> str:
    return value.strip().strip("`").split("<", 1)[0].rstrip(".,;:)")


def _registry_version(value: str) -> str:
    return VERSION_PREFIX_RE.sub("", value)


def _bump(name: str, old_version: str, new_version: str) -> Bump:
    return Bump(
        name=_clean_token(name),
        old_version=_clean_token(old_version),
        new_version=_clean_token(new_version),
    )


def _parse_metadata_updates(payload: str) -> tuple[Bump, ...]:
    if not payload:
        return ()
    try:
        raw_updates = json.loads(payload)
    except json.JSONDecodeError:
        return ()
    if not isinstance(raw_updates, list):
        return ()

    updates: list[Bump] = []
    for raw_update in raw_updates:
        if not isinstance(raw_update, dict):
            continue
        name = raw_update.get("dependencyName") or raw_update.get("dependency-name") or raw_update.get("name")
        old_version = (
            raw_update.get("previousVersion")
            or raw_update.get("previous-version")
            or raw_update.get("previous_version")
        )
        new_version = raw_update.get("newVersion") or raw_update.get("new-version") or raw_update.get("new_version")
        if isinstance(name, str) and isinstance(old_version, str) and isinstance(new_version, str):
            updates.append(_bump(name, old_version, new_version))
    return tuple(updates)


def _parse_grouped_bump(body: str, *, dependency_group: str = "", metadata_updates: tuple[Bump, ...] = ()) -> Bump | None:
    intro = body.split("<details>", 1)[0]
    group_match = GROUP_BODY_RE.search(intro)
    if not group_match and not dependency_group:
        return None

    if dependency_group:
        group = dependency_group
    elif group_match:
        group = group_match.group("group")
    else:
        return None
    body_updates = tuple(
        _bump(match.group("name"), match.group("old"), match.group("new"))
        for match in GROUP_UPDATE_RE.finditer(body)
    )
    updates = metadata_updates or body_updates
    if len(updates) < 2:
        return None
    return Bump(name=f"{group} group", old_version="", new_version="", updates=updates, group=group)


def _parse_bump(
    title: str,
    body: str,
    *,
    dependency_group: str = "",
    updated_dependencies_json: str = "",
) -> Bump | None:
    metadata_updates = _parse_metadata_updates(updated_dependencies_json)
    grouped_bump = _parse_grouped_bump(body, dependency_group=dependency_group, metadata_updates=metadata_updates)
    if grouped_bump:
        return grouped_bump

    title_match = BUMP_TITLE_RE.search(title)
    if title_match:
        return _bump(title_match.group(1), title_match.group(2), title_match.group(3))

    intro = body.split("<details>", 1)[0]
    body_match = BUMP_BODY_RE.search(intro)
    if body_match:
        return _bump(body_match.group("name"), body_match.group("old"), body_match.group("new"))
    return None


def _pypi_info(package: str, version: str) -> RegistryInfo | None:
    payload = _fetch_json(f"https://pypi.org/pypi/{package}/{version}/json")
    if not payload:
        return None
    info = payload.get("info", {})
    project_urls = info.get("project_urls") or {}
    return RegistryInfo(
        summary=(info.get("summary") or "").strip(),
        homepage=info.get("home_page") or _first_url(project_urls, "Homepage", "Home"),
        repository=_first_url(project_urls, "Repository", "Source", "Code", "GitHub"),
        changelog=_first_url(project_urls, "Changelog", "Release notes", "Changes"),
        documentation=info.get("docs_url") or _first_url(project_urls, "Documentation", "Docs"),
        license=(info.get("license") or "").strip() or None,
        published=(payload.get("urls") or [{}])[0].get("upload_time"),
    )


def _crates_info(crate: str, version: str) -> RegistryInfo | None:
    payload = _fetch_json(f"https://crates.io/api/v1/crates/{crate}/{version}")
    if not payload:
        return None
    version_info = payload.get("version") or {}
    crate_info = payload.get("crate") or {}
    return RegistryInfo(
        summary=(crate_info.get("description") or "").strip(),
        homepage=crate_info.get("homepage"),
        repository=crate_info.get("repository"),
        changelog=crate_info.get("documentation"),
        documentation=crate_info.get("documentation"),
        license=version_info.get("license"),
        published=version_info.get("created_at"),
    )


def _github_actions_info(action: str) -> RegistryInfo | None:
    repo = action.split("@", 1)[0]
    repo_url = f"https://github.com/{repo}"
    return RegistryInfo(
        summary=f"GitHub Action from `{repo}`.",
        homepage=repo_url,
        repository=repo_url,
        changelog=f"{repo_url}/releases",
        documentation=f"{repo_url}#readme",
        license=None,
        published=None,
    )


def _is_github_actions(ecosystem: str) -> bool:
    return ecosystem in {"github-actions", "github_actions"}


def _dependency_type_label(raw: str | None) -> str:
    if not raw:
        return "unknown"
    if "development" in raw:
        return "development"
    if "production" in raw:
        return "production"
    return raw


def _update_type_label(raw: str | None) -> str:
    if not raw:
        return "unknown"
    if raw.startswith("version-update:semver-"):
        return raw.removeprefix("version-update:semver-")
    if raw.startswith("semver_"):
        return raw.removeprefix("semver_")
    if raw.startswith("semver-"):
        return raw.removeprefix("semver-")
    return raw


def _build_title(
    bump: Bump,
    *,
    ecosystem: str,
    dependency_type: str,
    update_type: str,
    summary: str,
) -> str:
    dep_tag = "dev" if dependency_type == "development" else "prod"
    update_tag = _update_type_label(update_type)
    if bump.updates:
        update_label = f"{len(bump.updates)} {update_tag} updates" if update_tag != "unknown" else f"{len(bump.updates)} updates"
        return f"deps({dep_tag}): {bump.name} ({update_label}; {ecosystem})"

    short_summary = summary.split(".", 1)[0].strip() if summary else ""
    context = short_summary[:72] + ("…" if len(short_summary) > 72 else "") if short_summary else ecosystem
    return (
        f"deps({dep_tag}): {bump.name} {bump.old_version} → {bump.new_version} "
        f"({update_tag}; {context})"
    )


def _strip_existing_enrichment(body: str) -> str:
    if MARKER not in body:
        return body.strip()
    summary_index = body.find(ORIGINAL_SUMMARY)
    if summary_index == -1:
        return body.strip()
    original = body[summary_index + len(ORIGINAL_SUMMARY) :].strip()
    if original.endswith("</details>"):
        original = original[: -len("</details>")].strip()
    return original


def _write_skip_output() -> None:
    output_dir = os.environ.get("GITHUB_OUTPUT")
    if output_dir:
        with open(output_dir, "a", encoding="utf-8") as handle:
            handle.write("skip=true\n")


def _build_body(
    bump: Bump,
    *,
    ecosystem: str,
    dependency_type: str,
    update_type: str,
    info: RegistryInfo | None,
    original_body: str,
) -> str:
    original = _strip_existing_enrichment(original_body)
    versions = f"{len(bump.updates)} updates" if bump.updates else f"`{bump.old_version}` → `{bump.new_version}`"
    lines = [
        MARKER,
        "## Dependency update",
        "",
        "| Field | Value |",
        "| --- | --- |",
        f"| Package | `{bump.name}` |",
        f"| Ecosystem | {ecosystem} |",
        f"| Dependency type | {_dependency_type_label(dependency_type)} |",
        f"| Update type | {_update_type_label(update_type)} |",
        f"| Versions | {versions} |",
        "",
    ]
    if bump.updates:
        lines.extend(
            [
                "## Dependencies",
                "",
                "| Package | From | To |",
                "| --- | --- | --- |",
                *(
                    f"| `{update.name}` | `{update.old_version}` | `{update.new_version}` |"
                    for update in bump.updates
                ),
                "",
            ]
        )
    if info and info.summary:
        lines.extend(["## About", "", info.summary, ""])
    links: list[tuple[str, str]] = []
    if ecosystem == "uv":
        links.append(("PyPI", f"https://pypi.org/project/{bump.name}/{_registry_version(bump.new_version)}/"))
    elif ecosystem == "cargo":
        links.append(("crates.io", f"https://crates.io/crates/{bump.name}/{bump.new_version}"))
    if info:
        if info.homepage:
            links.append(("Homepage", info.homepage))
        if info.repository:
            links.append(("Repository", info.repository))
        if info.changelog:
            links.append(("Changelog / docs", info.changelog))
        if info.documentation and info.documentation not in {url for _, url in links}:
            links.append(("Documentation", info.documentation))
    if links:
        lines.extend(["## Links", ""])
        lines.extend(f"- [{label}]({url})" for label, url in links)
        lines.append("")
    meta_lines: list[str] = []
    if info and info.license:
        meta_lines.append(f"- License: {info.license}")
    if info and info.published:
        meta_lines.append(f"- Published: {info.published}")
    if meta_lines:
        lines.extend(["## Release metadata", "", *meta_lines, ""])
    lines.extend(["---", "", "<details>", "<summary>Original Dependabot description</summary>", "", original, "", "</details>"])
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--title", required=True)
    parser.add_argument("--body", default="")
    parser.add_argument("--ecosystem", default=os.environ.get("DEPENDABOT_ECOSYSTEM", ""))
    parser.add_argument("--dependency-type", default=os.environ.get("DEPENDABOT_DEPENDENCY_TYPE", ""))
    parser.add_argument("--update-type", default=os.environ.get("DEPENDABOT_UPDATE_TYPE", ""))
    parser.add_argument("--dependency-group", default=os.environ.get("DEPENDABOT_DEPENDENCY_GROUP", ""))
    parser.add_argument(
        "--updated-dependencies-json",
        default=os.environ.get("DEPENDABOT_UPDATED_DEPENDENCIES_JSON", ""),
    )
    parser.add_argument("--title-out", default="")
    parser.add_argument("--body-out", default="")
    args = parser.parse_args()

    original_body = _strip_existing_enrichment(args.body)

    bump = _parse_bump(
        args.title,
        original_body,
        dependency_group=args.dependency_group,
        updated_dependencies_json=args.updated_dependencies_json,
    )
    if bump is None:
        print("Could not parse dependency bump from PR title/body.", file=sys.stderr)
        return 1

    info: RegistryInfo | None
    if bump.updates:
        info = None
    elif args.ecosystem == "uv":
        info = _pypi_info(bump.name, _registry_version(bump.new_version))
    elif args.ecosystem == "cargo":
        info = _crates_info(bump.name, bump.new_version)
    elif _is_github_actions(args.ecosystem):
        info = _github_actions_info(bump.name)
    else:
        info = None

    title = _build_title(
        bump,
        ecosystem=args.ecosystem or "dependency",
        dependency_type=_dependency_type_label(args.dependency_type),
        update_type=args.update_type,
        summary=info.summary if info else "",
    )
    body = _build_body(
        bump,
        ecosystem=args.ecosystem or "dependency",
        dependency_type=args.dependency_type,
        update_type=args.update_type,
        info=info,
        original_body=original_body,
    )

    if title == args.title and body == args.body:
        _write_skip_output()
        print("PR already enriched; skipping.", file=sys.stderr)
        return 0

    if args.title_out:
        with open(args.title_out, "w", encoding="utf-8") as handle:
            handle.write(title)
    if args.body_out:
        with open(args.body_out, "w", encoding="utf-8") as handle:
            handle.write(body)

    output_dir = os.environ.get("GITHUB_OUTPUT")
    if output_dir:
        with open(output_dir, "a", encoding="utf-8") as handle:
            handle.write(f"title={title}\n")

    if not args.title_out and not args.body_out and not output_dir:
        print(title)
        print("---")
        print(body)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

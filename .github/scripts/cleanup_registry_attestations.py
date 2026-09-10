"""Remove legacy GHCR referrers indexes, preserving application image versions."""

import json
import os
import re
import subprocess


def is_legacy_version(version: dict) -> bool:
    tags = version.get("metadata", {}).get("container", {}).get("tags", [])
    return bool(
        re.fullmatch(r"sha256:[0-9a-f]{64}", version.get("name", ""))
        and tags
        and all(re.fullmatch(r"sha256-[0-9a-f]{64}", tag) for tag in tags)
    )


def is_attestation_index(manifest: dict) -> bool:
    descriptors = manifest.get("manifests", [])
    return bool(
        manifest.get("mediaType") == "application/vnd.oci.image.index.v1+json"
        and descriptors
        and all(
            descriptor.get("mediaType") == "application/vnd.oci.image.manifest.v1+json"
            and descriptor.get("artifactType") == "application/vnd.dev.sigstore.bundle.v0.3+json"
            and "platform" not in descriptor
            for descriptor in descriptors
        )
    )


def api_json(endpoint: str, *options: str):
    return json.loads(subprocess.check_output(
        ["gh", "api", "--method", "GET", endpoint, *options], text=True,
    ))


def read_manifest(image: str, digest: str) -> dict:
    return json.loads(subprocess.check_output([
        "docker", "buildx", "imagetools", "inspect", "--raw", f"{image}@{digest}",
    ], text=True))


def cleanup(repository: str, image: str) -> int:
    if image != f"ghcr.io/{repository.lower()}":
        raise ValueError("Only the current repository's GHCR package can be cleaned")
    owner, package = repository.split("/")
    namespace = "orgs" if api_json(f"repos/{repository}")["owner"]["type"] == "Organization" else "users"
    endpoint = f"{namespace}/{owner}/packages/container/{package.lower()}/versions"
    # Finish pagination before deleting, so removals cannot shift later pages.
    pages = api_json(f"{endpoint}?per_page=100", "--paginate", "--slurp")
    versions = [version for page in pages for version in page if is_legacy_version(version)]
    removed = 0
    for version in versions:
        if not is_attestation_index(read_manifest(image, version["name"])):
            continue
        version_endpoint = f"{endpoint}/{version['id']}"
        # Recheck aliases immediately before deleting the package version.
        # A version with latest, a release tag, or an unknown alias is retained.
        current = api_json(version_endpoint)
        if not is_legacy_version(current) or current["name"] != version["name"]:
            continue
        subprocess.run(["gh", "api", "--method", "DELETE", version_endpoint], check=True)
        print(f"Removed legacy attestation index {version['name']}")
        removed += 1
    return removed


if __name__ == "__main__":
    count = cleanup(os.environ["GITHUB_REPOSITORY"], os.environ["IMAGE_NAME"])
    print(f"Removed {count} legacy GHCR attestation index(es).")

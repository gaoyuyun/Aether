import contextlib
import copy
import io
import subprocess
import unittest
from unittest.mock import patch

from cleanup_registry_attestations import cleanup, is_attestation_index, is_legacy_version


DIGEST = "sha256:" + "a" * 64
TAG = "sha256-" + "b" * 64
VERSION = {"id": 123, "name": DIGEST, "metadata": {"container": {"tags": [TAG]}}}
INDEX = {
    "mediaType": "application/vnd.oci.image.index.v1+json",
    "manifests": [{
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": "application/vnd.dev.sigstore.bundle.v0.3+json",
    }],
}


class AttestationCleanupTests(unittest.TestCase):
    def test_release_and_unknown_aliases_are_always_preserved(self):
        self.assertTrue(is_legacy_version(VERSION))
        for tags in [[], ["latest", TAG], ["v0.7.19", TAG], ["sha-abcdef"], [TAG, "keep"]]:
            with self.subTest(tags=tags):
                version = copy.deepcopy(VERSION)
                version["metadata"]["container"]["tags"] = tags
                self.assertFalse(is_legacy_version(version))

    def test_only_a_nonempty_index_of_sigstore_bundles_is_eligible(self):
        self.assertTrue(is_attestation_index(INDEX))
        runtime = {"mediaType": "application/vnd.oci.image.manifest.v1+json", "platform": {"os": "linux", "architecture": "amd64"}}
        for descriptors in [[], [runtime], INDEX["manifests"] + [runtime], [{}]]:
            with self.subTest(descriptors=descriptors):
                self.assertFalse(is_attestation_index({**INDEX, "manifests": descriptors}))
        self.assertFalse(is_attestation_index({}))

    def test_cleanup_inspects_content_and_rechecks_aliases_before_deletion(self):
        responses = [{"owner": {"type": "User"}}, [[VERSION]], VERSION]
        with patch("cleanup_registry_attestations.api_json", side_effect=responses):
            with patch("cleanup_registry_attestations.read_manifest", return_value=INDEX) as inspect:
                with patch("cleanup_registry_attestations.subprocess.run") as delete:
                    with contextlib.redirect_stdout(io.StringIO()):
                        self.assertEqual(cleanup("owner/App", "ghcr.io/owner/app"), 1)
                    inspect.assert_called_once_with("ghcr.io/owner/app", DIGEST)
                    delete.assert_called_once_with([
                        "gh", "api", "--method", "DELETE", "users/owner/packages/container/app/versions/123",
                    ], check=True)

    def test_cleanup_retains_a_version_that_acquired_a_release_alias(self):
        current = copy.deepcopy(VERSION)
        current["metadata"]["container"]["tags"].append("v0.7.19.1")
        responses = [{"owner": {"type": "User"}}, [[VERSION]], current]
        with patch("cleanup_registry_attestations.api_json", side_effect=responses):
            with patch("cleanup_registry_attestations.read_manifest", return_value=INDEX):
                with patch("cleanup_registry_attestations.subprocess.run") as delete:
                    self.assertEqual(cleanup("owner/App", "ghcr.io/owner/app"), 0)
                    delete.assert_not_called()

    def test_sha_named_application_image_is_not_deleted(self):
        responses = [{"owner": {"type": "Organization"}}, [[VERSION]]]
        with patch("cleanup_registry_attestations.api_json", side_effect=responses):
            with patch("cleanup_registry_attestations.read_manifest", return_value={"layers": []}):
                with patch("cleanup_registry_attestations.subprocess.run") as delete:
                    self.assertEqual(cleanup("owner/App", "ghcr.io/owner/app"), 0)
                    delete.assert_not_called()

    def test_failed_inspection_cannot_delete_anything(self):
        responses = [{"owner": {"type": "User"}}, [[VERSION]]]
        with patch("cleanup_registry_attestations.api_json", side_effect=responses):
            with patch("cleanup_registry_attestations.read_manifest", side_effect=subprocess.CalledProcessError(1, "docker")):
                with patch("cleanup_registry_attestations.subprocess.run") as delete:
                    with self.assertRaises(subprocess.CalledProcessError):
                        cleanup("owner/App", "ghcr.io/owner/app")
                    delete.assert_not_called()


if __name__ == "__main__":
    unittest.main()

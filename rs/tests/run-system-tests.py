#!/usr/bin/env python3
"""
A runner of farm-based system tests.
Essentially it is a wrapper around `prod-test-driver`, which executes test suites.
This script can be used both locally and on the CI.

Responsibilities:
- Download/extract artifacts and guest-os image.
- Generate ssh keys.
- Run the test driver.
- Push results to honeycomb.
- Send slack messages (for scheduled jobs) about the failed tests.

Typical example usage:

    ./run-system-tests.py --suite=pre_master
"""
import getpass
import gzip
import json
import logging
import os
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
from contextlib import contextmanager
from pathlib import Path
from typing import Dict
from typing import List
from typing import Tuple

import requests
import requests.packages.urllib3.util.connection as urllib3_cn


logging.basicConfig(level=logging.DEBUG, format="%(levelname)s:%(name)s:%(message)s")


RED = "\033[1;31m"
GREEN = "\033[1;32m"
NC = "\033[0m"
TIMEOUT_CODE = 124

# This timeout should be shorter than the CI job timeout.
TIMEOUT_DEFAULT_SEC = 50 * 60
GET_EXTERNAL_IPV6_TIMEOUT = 10
SLACK_CHANNEL_NOTIFY = "test-failure-alerts"
RUN_TESTS_SUBCOMMAND = "run-tests"
PROCESS_TEST_RESULTS_SUBCOMMAND = "process-test-results"
TEST_RESULT_FILE = "test-results.json"
POT_SETUP_FILE = "group_setup.json"
POT_SETUP_RESULT_FILE = "pot_setup_result.json"
SLACK_FAILURE_ALERTS_FILE = "slack_alerts.json"
SHELL_WRAPPER_DEFAULT = "/usr/bin/time"
DEFAULT_FARM_BASE_URL = "https://farm.dfinity.systems"


class GetImageShaException(Exception):
    pass


class GetExternalIPv6Exception(Exception):
    pass


def try_extract_arguments(search_args: List[str], separator: str, args: List[str]) -> Tuple[str, ...]:
    arg_values: List[str] = []
    for search_arg in search_args:
        search_result = [search_arg in arg for arg in args]
        arg_value = ""
        try:
            idx = search_result.index(True)
            _, arg_value = args[idx].split(separator)
        except ValueError:
            pass
        arg_values.append(arg_value)
    return tuple(arg_values)


def try_kill_pgid(pgid: int) -> None:
    # If the process has already exited, then ProcessLookupError will be raised.
    # We simply ignore this specific exception.
    try:
        os.killpg(pgid, signal.SIGTERM)
    except ProcessLookupError:
        pass


def notify_slack(slack_message: str, ci_project_dir: str, channel: str) -> int:
    notify_slack_command = [
        "python3",
        f"{ci_project_dir}/gitlab-ci/src/notify_slack/notify_slack.py",
        slack_message,
        f"--channel={channel}",
    ]
    returncode = run_command(command=notify_slack_command)
    return returncode


def _test_driver_local_run_cmd() -> List[str]:
    return ["cargo", "run", "--bin", "prod-test-driver", "--"]


def run_help_command(shell_wrapper: str):
    # Help command is supposed to be run only locally.
    help_command = [shell_wrapper] + _test_driver_local_run_cmd() + ["--help"]
    run_command(command=help_command)


def exit_with_log(msg: str) -> None:
    logging.error(f"{RED}{msg}{NC}")
    sys.exit(1)


def extract_artifacts(source_dir: str, dest_dir: str, is_set_executable: bool) -> None:
    files_list = os.listdir(source_dir)
    logging.info(f"Unzipping {len(files_list)} files in {source_dir} dir.")
    for file in files_list:
        file_name = os.path.join(source_dir, file)
        if file_name.endswith(".gz"):
            with gzip.open(file_name, "rb") as f_in:
                # Take filename without extension.
                save_file = os.path.splitext(file_name)[0]
                with open(save_file, "wb") as f_out:
                    shutil.copyfileobj(f_in, f_out)
                    # Set executable attribute (chmod +x).
                    if is_set_executable:
                        st = os.stat(save_file)
                        os.chmod(save_file, st.st_mode | stat.S_IEXEC)
                    # Move the file after extraction (overwrite if exists).
                    shutil.move(save_file, os.path.join(dest_dir, os.path.basename(save_file)))


def replace_symbols(text: str, symbols_to_replace: List[str], replace_with: str) -> str:
    for ch in symbols_to_replace:
        text = text.replace(ch, replace_with)
    return text


def remove_folders(folders: List[str]) -> None:
    for folder in folders:
        logging.info(f"{RED}Removing directory {folder}.{NC}")
        shutil.rmtree(folder, ignore_errors=True)


def create_env_variables(is_local_run: bool, artifact_dir: str, ci_project_dir: str, tmp_dir: str) -> Dict:
    env = os.environ.copy()
    env["TMPDIR"] = tmp_dir
    env["PATH"] = f"{artifact_dir}:" + env["PATH"]
    env["PATH"] = f"{ci_project_dir}/rs/tests:" + env["PATH"]
    if not is_local_run:
        env["XNET_TEST_CANISTER_WASM_PATH"] = f"{artifact_dir}/xnet-test-canister.wasm"
    slack_notify = f"{ci_project_dir}/gitlab-ci/src/notify_slack"
    if env.get("PYTHONPATH") is None:
        env.setdefault("PYTHONPATH", slack_notify)
    else:
        env["PYTHONPATH"] = f"{slack_notify}:" + env["PYTHONPATH"]
    return env


def get_current_commit_sha() -> str:
    cmd = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True)
    if cmd.returncode == 0:
        commit_sha = cmd.stdout.decode("UTF-8").strip()
        logging.info(f"current commit={commit_sha}")
        return commit_sha
    else:
        exit_with_log("Couldn't get the current commit hash")


def get_commit_date(commit_sha: str) -> str:
    cmd = subprocess.run(["git", "show", "-s", "--format=%cD", f"{commit_sha}"], capture_output=True)
    if cmd.returncode == 0:
        datetime = cmd.stdout.decode("UTF-8").strip()
        logging.info(f"commit={commit_sha} datetime: {datetime}")
        return datetime
    else:
        logging.error(f"{RED}Couldn't get datetime of the commit={commit_sha}: {cmd.stderr.decode('UTF-8')}{NC}")
        return "unresolved commit datetime"


def get_ic_os_image_sha(img_base_url, filename="disk-img.tar.zst") -> Tuple[str, str]:
    img_url = f"{img_base_url}{filename}"
    img_sha256_url = f"{img_base_url}SHA256SUMS"
    result = requests.get(f"{img_sha256_url}")
    logging.debug(f"GET {img_sha256_url} responded with status_code={result.status_code}.")
    if result.status_code != 200:
        raise GetImageShaException(f"Unexpected status_code={result.status_code} for the GET {img_sha256_url}")
    try:
        hashes = {}
        for line in result.text.splitlines():
            parts = line.split(" ")
            sha256hex = parts[0]
            name = parts[1][1:]
            hashes[name] = sha256hex
        img_sha256 = hashes[filename]
        return img_sha256, img_url
    except Exception:
        raise GetImageShaException(f"Couldn't extract img_sha256 from {result.text}.")


def run_command(command: List[str], **kwargs) -> int:
    process = subprocess.run(command, **kwargs)
    return process.returncode


def run_command_with_timeout(command: List[str], timeout=None, **kwargs) -> int:
    # As the command below launches more than one subprocess, we need to use Popen to have control over child processes.
    # In particular, we use a new_session to kill the whole group of processes at once.
    try:
        p = subprocess.Popen(command, start_new_session=True, **kwargs)
        pgid = os.getpgid(p.pid)
        p.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        # Normally, in case of TimeoutExpired return code is not set.
        p.returncode = p.returncode if p.returncode is not None else TIMEOUT_CODE
    finally:
        # Kill the whole process group.
        try_kill_pgid(pgid)
    return p.returncode


def generate_default_job_id() -> str:
    return f"{getpass.getuser()}-{socket.gethostname()}-{int(time.time())}"


def build_test_driver(shell_wrapper: str) -> int:
    test_driver_build_cmd = [shell_wrapper, "cargo", "build", "--bin", "prod-test-driver"]
    logging.info("Building prod-test-driver binary...")
    status_code = run_command(command=test_driver_build_cmd)
    return status_code


def send_all_slack_alerts(slack_alerts_file_path: str, ci_project_dir: str) -> None:
    with open(slack_alerts_file_path, "r") as f:
        alerts = json.load(f)
    for id, alert in alerts.items():
        returncode = notify_slack(
            slack_message=alert["message"], ci_project_dir=ci_project_dir, channel=alert["channel"]
        )
        if returncode != 0:
            logging.error(
                f"Failed to send slack alert with id={id} to channel={alert['channel']}, exit code={returncode}."
            )


def allowed_gai_family():
    if not urllib3_cn.HAS_IPV6:
        raise Exception("IPv6 is required but not supported")
    return socket.AF_INET6


@contextmanager
def use_ipv6_in_requests():
    orig_allowed_gai_family = urllib3_cn.allowed_gai_family
    try:
        urllib3_cn.allowed_gai_family = allowed_gai_family
        yield
    finally:
        urllib3_cn.allowed_gai_family = orig_allowed_gai_family


def get_external_ipv6_address():
    url = "http://ifconfig.co"
    try:
        with use_ipv6_in_requests():
            get_ipv6_resp = requests.get(url, timeout=GET_EXTERNAL_IPV6_TIMEOUT, headers={"accept": "text/plain"})
        status_code = get_ipv6_resp.status_code
        if status_code != 200:
            logging.warning(
                f"Failed to get external IPv6 address by requesting {url} because status code = {str(status_code)}!"
            )
            return None
        return get_ipv6_resp.text.strip()
    except BaseException as err:
        logging.warning(f"Failed to get external IPv6 address by requesting {url} because: {err}")
        return None


def list_files(root_path: str) -> None:
    for root, _, files in os.walk(root_path):
        level = root.replace(root_path, "").count(os.sep)
        indent = " " * 4 * (level)
        logging.debug("{}{}/".format(indent, os.path.basename(root)))
        sub_indent = " " * 4 * (level + 1)
        for f in files:
            logging.debug("{}{}".format(sub_indent, f))


def populate_dependencies_dir(
    dependencies_dir: str,
    artifacts_dir: str,
    ic_root_dir: str,
    journalbeat_hosts: str,
    farm_base_url: str,
    replica_log_debug_overrides: str,
    ic_version_id: str,
    ic_os_img_url: str,
    ic_os_img_sha256: str,
    ic_os_update_img_url: str,
    ic_os_update_img_sha256: str,
    boundary_node_snp_img_url: str,
    boundary_node_snp_img_sha256: str,
    boundary_node_img_url: str,
    boundary_node_img_sha256: str,
) -> None:
    # Start: create symlinks for scripts
    scripts_rel_paths = [
        "ic-os/guestos/scripts/build-bootstrap-config-image.sh",
        "ic-os/boundary-guestos/scripts/build-bootstrap-config-image.sh",
        "rs/tests/create-universal-vm-config-image.sh",
        "rs/tests/rosetta_workspace/ic_rosetta_api_log_config.yml",
        "rs/tests/rosetta_workspace/rosetta_cli.json",
        "rs/tests/rosetta_workspace/rosetta_workflows.ros",
        "rs/tests/src/canister_http/universal_vm_activation.sh",
        "ic-os/guestos/rootfs/dev-certs/canister_http_test_ca.cert",
        "ic-os/guestos/rootfs/dev-certs/canister_http_test_ca.key",
    ]

    for src_rel_path in scripts_rel_paths:
        src = os.path.join(ic_root_dir, src_rel_path)
        dst = os.path.join(dependencies_dir, src_rel_path)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        os.symlink(src, dst)
    # End: create symlinks for scripts

    # Start: create files with content
    files_with_content = [
        ("farm_base_url", farm_base_url),
        ("journalbeat_hosts", journalbeat_hosts),
        ("replica_log_debug_overrides", replica_log_debug_overrides),
        ("bazel/version.txt", ic_version_id),
        ("ic-os/guestos/dev/upload_disk-img_disk-img.tar.zst.proxy-cache-url", ic_os_img_url),
        ("ic-os/guestos/dev/disk-img.tar.zst.sha256", ic_os_img_sha256),
        ("ic-os/guestos/dev/upload_update-img_update-img.tar.zst.proxy-cache-url", ic_os_update_img_url),
        ("ic-os/guestos/dev/update-img.tar.zst.sha256", ic_os_update_img_sha256),
        ("ic-os/boundary-guestos/boundary_node_img_url", boundary_node_img_url),
        ("ic-os/boundary-guestos/boundary_node_img_sha256", boundary_node_img_sha256),
        ("ic-os/boundary-guestos/boundary_node_snp_img_url", boundary_node_snp_img_url),
        ("ic-os/boundary-guestos/boundary_node_snp_img_sha256", boundary_node_snp_img_sha256),
    ]

    for (file_rel_path, content) in files_with_content:
        dst = os.path.join(dependencies_dir, file_rel_path)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        if content:
            with open(dst, "w") as f:
                f.write(content)
    # End: create files with content

    # Create symlinks to $artifacts_dir/$key from $dependencies_dir/$value
    links = {
        # NNS canisters
        "registry-canister.wasm": "rs/tests/nns-canisters/registry-canister",
        "governance-canister_test.wasm": "rs/tests/nns-canisters/governance-canister_test",
        "ledger-canister_notify-method.wasm": "rs/tests/nns-canisters/ledger-canister_notify-method",
        "root-canister.wasm": "rs/tests/nns-canisters/root-canister",
        "cycles-minting-canister.wasm": "rs/tests/nns-canisters/cycles-minting-canister",
        "lifeline.wasm": "rs/tests/nns-canisters/lifeline",
        "genesis-token-canister.wasm": "rs/tests/nns-canisters/genesis-token-canister",
        "sns-wasm-canister.wasm": "rs/tests/nns-canisters/sns-wasm-canister",
        "ic-icrc1-ledger.wasm": "rs/rosetta-api/icrc1/ledger/ledger_canister.wasm",
        "ic-ckbtc-minter.wasm": "rs/bitcoin/ckbtc/minter/ckbtc_minter.wasm",
        "ic-ckbtc-minter_debug.wasm": "rs/bitcoin/ckbtc/minter/ckbtc_minter_debug.wasm",
        "ic-rosetta-api": "rs/rosetta-api/ic-rosetta-api",
        "http_counter.wasm": "rs/tests/test_canisters/http_counter/http_counter.wasm",
        "kv_store.wasm": "rs/tests/test_canisters/kv_store/kv_store.wasm",
        "counter.wat": "rs/workload_generator/src/counter.wat",
        "proxy_canister.wasm": "rs/rust_canisters/proxy_canister/proxy_canister.wasm",
    }
    for source, dest in links.items():
        dst = os.path.join(dependencies_dir, dest)
        dirname = os.path.dirname(dst)
        os.makedirs(dirname, exist_ok=True)
        src = os.path.join(artifacts_dir, source)
        os.symlink(src, dst)  # dst -> src

    # See ./prepare-rosetta-deps.sh
    ROSETTA_CLI_DIR = os.getenv("ROSETTA_CLI_DIR")
    if ROSETTA_CLI_DIR is not None:
        rosetta_cli_dir = os.path.join(dependencies_dir, "external/rosetta-cli")
        os.makedirs(rosetta_cli_dir)
        os.symlink(
            os.path.join(ROSETTA_CLI_DIR, "rosetta-cli"),
            os.path.join(rosetta_cli_dir, "rosetta-cli"),
        )


def main(
    runner_args: List[str], working_dir: str, folders_to_remove: List[str], keep_tmp_artifacts_folder: bool
) -> int:
    # From this path the script was started.
    base_path = os.getcwd()
    # Set path to the script path (in case script is launched from non-parent dir).
    current_path = Path(os.path.dirname(os.path.abspath(__file__)))
    os.chdir(current_path.absolute())
    root_ic_dir = str(current_path.parent.parent.absolute())
    # Read all environmental variables.
    CI_PROJECT_DIR = os.getenv("CI_PROJECT_DIR", default=root_ic_dir)
    TEST_ES_HOSTNAMES = os.getenv("TEST_ES_HOSTNAMES", default=None)
    SHELL_WRAPPER = os.getenv("SHELL_WRAPPER", default=SHELL_WRAPPER_DEFAULT)
    SSH_KEY_DIR = os.getenv("SSH_KEY_DIR", default=None)
    IC_VERSION_ID = os.getenv("IC_VERSION_ID", default="")
    GUESTOS_VERSION_OVERRIDE = os.getenv("GUESTOS_VERSION_OVERRIDE", default=IC_VERSION_ID)
    JOB_ID = os.getenv("CI_JOB_ID", default=None)
    CI_PARENT_PIPELINE_SOURCE = os.getenv("CI_PARENT_PIPELINE_SOURCE", default="")
    CI_PIPELINE_SOURCE = os.getenv("CI_PIPELINE_SOURCE", default="")
    ROOT_PIPELINE_ID = os.getenv("ROOT_PIPELINE_ID", default="")
    CI_JOB_URL = os.getenv("CI_JOB_URL", default="")
    CI_PROJECT_URL = os.getenv("CI_PROJECT_URL", default="")
    CI_COMMIT_SHA = os.getenv("CI_COMMIT_SHA", default="")
    CI_COMMIT_SHORT_SHA = os.getenv("CI_COMMIT_SHORT_SHA", default="")
    ARTIFACT_DIR = os.getenv("ARTIFACT_DIR", default="")
    CI_JOB_NAME = os.getenv("CI_JOB_NAME", default="")
    SYSTEM_TESTS_TIMEOUT_SEC = int(os.getenv("SYSTEM_TESTS_TIMEOUT", default=TIMEOUT_DEFAULT_SEC))
    logging.info(f"{RED}Test suite execution timeout is {SYSTEM_TESTS_TIMEOUT_SEC} sec.{NC}")
    # Start set variables.
    is_local_run = JOB_ID is None
    use_locally_prebuilt_artifacts = ARTIFACT_DIR != ""
    # Handle relative ARTIFACT_DIR path.
    if is_local_run and not os.path.isabs(ARTIFACT_DIR):
        ARTIFACT_DIR = os.path.join(base_path, ARTIFACT_DIR)
    is_merge_request = CI_PARENT_PIPELINE_SOURCE == "merge_request_event"
    is_honeycomb_push = not is_local_run
    is_scheduled_run = CI_PIPELINE_SOURCE == "schedule"
    is_slack_test_failure_notify = not is_local_run and is_scheduled_run
    is_slack_timeout_notify = not is_local_run and is_scheduled_run
    ic_version_id_date = get_commit_date(IC_VERSION_ID)
    current_commit = CI_COMMIT_SHA if not is_local_run else get_current_commit_sha()
    commit_date = get_commit_date(current_commit)
    # End set variables.

    # Firstly, build the prod-test-driver binary.
    if is_local_run:
        return_code = build_test_driver(SHELL_WRAPPER)
        if return_code != 0:
            exit_with_log("Failed to build prod-test-driver bin.")

    if not is_local_run and use_locally_prebuilt_artifacts:
        exit_with_log("One can't use locally prebuilt artifacts on the CI.")

    logging.debug(
        f"is_local_run={is_local_run}, is_merge_request={is_merge_request}, "
        f"use_locally_prebuilt_artifacts={use_locally_prebuilt_artifacts}, is_honeycomb_push={is_honeycomb_push}, "
        f"is_slack_test_failure_notify={is_slack_test_failure_notify}, is_slack_timeout_notify={is_slack_timeout_notify}"
    )

    if not IC_VERSION_ID:
        exit_with_log(
            "You must specify GuestOS image version via IC_VERSION_ID. You have two options:\n1) To obtain a GuestOS "
            "image version for your commit, please push your branch to origin and create an MR. See "
            "http://go/guestos-image-version\n2) To obtain the latest GuestOS image version for origin/master (e.g., "
            "if your changes are withing ic/rs/tests), use the following command: "
            "ic/gitlab-ci/src/artifacts/newest_sha_with_disk_image.sh origin/master\nNote: this command is not "
            "guaranteed to be deterministic."
        )

    if TEST_ES_HOSTNAMES is None:
        logging.info("TEST_ES_HOSTNAMES variable is not set, using defaults.")
        TEST_ES_HOSTNAMES = ",".join(
            [
                "elasticsearch-node-0.testnet.dfinity.systems:443",
                "elasticsearch-node-1.testnet.dfinity.systems:443",
                "elasticsearch-node-2.testnet.dfinity.systems:443",
            ]
        )
    TEST_ES_HOSTNAMES = replace_symbols(text=TEST_ES_HOSTNAMES, symbols_to_replace=["`", "'", " "], replace_with="")

    GUESTOS_BASE_URL = f"http://download.proxy-global.dfinity.network:8080/ic/{GUESTOS_VERSION_OVERRIDE}"
    IMG_BASE_URL = f"http://download.proxy-global.dfinity.network:8080/ic/{IC_VERSION_ID}"
    IC_OS_DEV_IMG_SHA256, IC_OS_DEV_IMG_URL = get_ic_os_image_sha(f"{GUESTOS_BASE_URL}/guest-os/disk-img-dev/")
    BOUNDARY_NODE_IMG_SHA256, BOUNDARY_NODE_IMG_URL = get_ic_os_image_sha(f"{IMG_BASE_URL}/boundary-os/disk-img-dev/")
    BOUNDARY_NODE_SNP_IMG_SHA256, BOUNDARY_NODE_SNP_IMG_URL = get_ic_os_image_sha(
        f"{IMG_BASE_URL}/boundary-os/disk-img-snp-dev/"
    )
    IC_OS_UPD_DEV_IMG_SHA256, IC_OS_UPD_DEV_IMG_URL = get_ic_os_image_sha(
        f"{IMG_BASE_URL}/guest-os/update-img-dev/", filename="update-img.tar.zst"
    )

    if SSH_KEY_DIR is None:
        logging.info("SSH_KEY_DIR variable is not set, generating keys.")
        SSH_KEY_DIR = tempfile.mkdtemp(prefix="tmp_ssh_keys_")
        folders_to_remove.append(SSH_KEY_DIR)
        gen_key_command = ["ssh-keygen", "-t", "ed25519", "-N", "", "-f", f"{SSH_KEY_DIR}/admin"]
        logging.debug(f"gen_key_command: {gen_key_command}")
        gen_key_returncode = run_command(command=gen_key_command)
        if gen_key_returncode == 0:
            logging.info("ssh keys generated successfully.")
        else:
            exit_with_log("Failed to generate ssh keys.")

    if is_local_run:
        JOB_ID = generate_default_job_id()
        RUN_CMD = _test_driver_local_run_cmd()
        artifacts_tmp_dir = tempfile.mkdtemp(prefix="tmp_artifacts_")
        if not keep_tmp_artifacts_folder:
            folders_to_remove.append(artifacts_tmp_dir)
        _tmp = f"{artifacts_tmp_dir}/artifacts"
        if use_locally_prebuilt_artifacts:
            logging.info(f"Copying prebuilt artifacts from {ARTIFACT_DIR} to {_tmp}")
            shutil.copytree(ARTIFACT_DIR, _tmp)
        ARTIFACT_DIR = _tmp
    else:
        ARTIFACT_DIR = f"{CI_PROJECT_DIR}/artifacts"
        RUN_CMD = [f"{ARTIFACT_DIR}/prod-test-driver"]

    RESULT_FILE = f"{working_dir}/{TEST_RESULT_FILE}"

    SUMMARY_ARGS = [
        f"--test_results={RESULT_FILE}",
        f"--working_dir={working_dir}",
        f"--pot_setup_file={POT_SETUP_FILE}",
        f"--pot_setup_result_file={POT_SETUP_RESULT_FILE}",
    ]

    if is_local_run:
        SUMMARY_ARGS.append("--verbose")

    canisters_path = os.path.join(CI_PROJECT_DIR, f"{ARTIFACT_DIR}/canisters")
    release_path = os.path.join(CI_PROJECT_DIR, f"{ARTIFACT_DIR}/release")
    icos_path = os.path.join(CI_PROJECT_DIR, f"{ARTIFACT_DIR}/icos")

    logging.info(f"Artifacts will be stored in: {ARTIFACT_DIR}.")

    # For an easy deletion of all artifact folders produced by the `prod-test-driver` process,
    # we create a dedicated tmp directory for this process and set TMPDIR env variable.
    test_driver_tmp_dir = tempfile.mkdtemp(prefix="tmp_test_driver_")
    folders_to_remove.extend([test_driver_tmp_dir])

    env_dict = create_env_variables(
        is_local_run=is_local_run,
        artifact_dir=ARTIFACT_DIR,
        ci_project_dir=CI_PROJECT_DIR,
        tmp_dir=test_driver_tmp_dir,
    )

    # Print all input environmental variables.
    logging.debug(
        f"CI_PROJECT_DIR={CI_PROJECT_DIR}, TEST_ES_HOSTNAMES={TEST_ES_HOSTNAMES}, SHELL_WRAPPER={SHELL_WRAPPER}, "
        f"SSH_KEY_DIR={SSH_KEY_DIR}, IC_VERSION_ID={IC_VERSION_ID}, JOB_ID={JOB_ID}, "
        f"CI_PIPELINE_SOURCE={CI_PIPELINE_SOURCE}, ROOT_PIPELINE_ID={ROOT_PIPELINE_ID}, CI_JOB_URL={CI_JOB_URL}, "
        f"CI_PROJECT_URL={CI_PROJECT_URL}, DEV_IMG_URL={IC_OS_DEV_IMG_URL}, CI_COMMIT_SHA={CI_COMMIT_SHA}, ARTIFACT_DIR={ARTIFACT_DIR}, "
        f"DEV_IMG_SHA256={IC_OS_DEV_IMG_SHA256}, CI_PARENT_PIPELINE_SOURCE={CI_PARENT_PIPELINE_SOURCE}, CI_JOB_NAME={CI_JOB_NAME}, "
        f"BOUNDARY_NODE_IMG_URL={BOUNDARY_NODE_IMG_URL}, BOUNDARY_NODE_IMG_SHA256={BOUNDARY_NODE_IMG_SHA256}, "
        f"BOUNDARY_NODE_SNP_IMG_URL={BOUNDARY_NODE_SNP_IMG_URL}, BOUNDARY_NODE_SNP_IMG_SHA256={BOUNDARY_NODE_SNP_IMG_SHA256}, "
        f"SYSTEM_TESTS_TIMEOUT_SEC={SYSTEM_TESTS_TIMEOUT_SEC}"
    )

    if use_locally_prebuilt_artifacts:
        logging.info(f"Extracting artifacts from the locally prebuilt {ARTIFACT_DIR} dir.")
        extract_artifacts(source_dir=canisters_path, dest_dir=ARTIFACT_DIR, is_set_executable=False)
        extract_artifacts(source_dir=icos_path, dest_dir=ARTIFACT_DIR, is_set_executable=False)
        extract_artifacts(source_dir=release_path, dest_dir=ARTIFACT_DIR, is_set_executable=True)
    elif is_merge_request:
        logging.info(f"Extracting artifacts from {ARTIFACT_DIR} dir.")
        extract_artifacts(source_dir=canisters_path, dest_dir=ARTIFACT_DIR, is_set_executable=False)
        extract_artifacts(source_dir=release_path, dest_dir=ARTIFACT_DIR, is_set_executable=True)
        if not keep_tmp_artifacts_folder:
            folders_to_remove.extend([canisters_path, release_path])
    else:
        logging.info(f"Downloading dependencies built from commit: {GREEN}{IC_VERSION_ID}{NC}")
        RCLONE_ARGS = [f"--git-rev={IC_VERSION_ID}", f"--out={ARTIFACT_DIR}", "--unpack", "--mark-executable"]
        clone_artifacts_canisters_cmd = [
            f"{CI_PROJECT_DIR}/gitlab-ci/src/artifacts/rclone_download.py",
            "--remote-path=canisters",
        ] + RCLONE_ARGS
        clone_artifacts_release_cmd = [
            f"{CI_PROJECT_DIR}/gitlab-ci/src/artifacts/rclone_download.py",
            "--remote-path=release",
        ] + RCLONE_ARGS
        logging.debug(f"clone_artifacts_canisters_cmd: {clone_artifacts_canisters_cmd}")
        logging.debug(f"clone_artifacts_release_cmd: {clone_artifacts_release_cmd}")
        download_canisters_returncode = run_command(command=clone_artifacts_canisters_cmd)
        download_release_returncode = run_command(command=clone_artifacts_release_cmd)
        if download_canisters_returncode != 0:
            logging.error(f"{RED}Failed to download canisters artifacts.{NC}")
        if download_release_returncode != 0:
            logging.error(f"{RED}Failed to download release artifacts.{NC}")

    logging.debug(f"ARTIFACT_DIR = {ARTIFACT_DIR} content:")
    list_files(ARTIFACT_DIR)

    dependencies_dir = os.path.join(working_dir, "system_env/dependencies")
    logging.info(f"Populating dependencies dir {dependencies_dir}")

    (replica_log_debug_overrides,) = try_extract_arguments(
        search_args=["--replica-log-debug-overrides"], separator="=", args=runner_args
    )

    populate_dependencies_dir(
        dependencies_dir=dependencies_dir,
        artifacts_dir=ARTIFACT_DIR,
        ic_root_dir=CI_PROJECT_DIR,
        ic_os_img_url=IC_OS_DEV_IMG_URL,
        ic_os_img_sha256=IC_OS_DEV_IMG_SHA256,
        ic_os_update_img_url=IC_OS_UPD_DEV_IMG_URL,
        ic_os_update_img_sha256=IC_OS_UPD_DEV_IMG_SHA256,
        ic_version_id=IC_VERSION_ID,
        journalbeat_hosts=TEST_ES_HOSTNAMES,
        boundary_node_snp_img_sha256=BOUNDARY_NODE_SNP_IMG_SHA256,
        boundary_node_snp_img_url=BOUNDARY_NODE_SNP_IMG_URL,
        farm_base_url=DEFAULT_FARM_BASE_URL,
        boundary_node_img_url=BOUNDARY_NODE_IMG_URL,
        boundary_node_img_sha256=BOUNDARY_NODE_IMG_SHA256,
        replica_log_debug_overrides=replica_log_debug_overrides,
    )
    logging.debug("dependencies dir has been populated with content:")
    list_files(dependencies_dir)

    run_test_driver_cmd = (
        [SHELL_WRAPPER]
        + RUN_CMD
        + [RUN_TESTS_SUBCOMMAND]
        + runner_args
        + [
            f"--job-id={JOB_ID}",
            f"--authorized-ssh-accounts={SSH_KEY_DIR}",
        ]
    )
    logging.debug(f"run_test_driver_cmd: {run_test_driver_cmd}")
    # We launch prod-test-driver with a timeout. This enables us to send slack notification before the global CI timeout kills the whole job.
    testrun_returncode = run_command_with_timeout(
        command=run_test_driver_cmd, env=env_dict, timeout=SYSTEM_TESTS_TIMEOUT_SEC
    )
    if testrun_returncode == 0:
        logging.info("Execution of the `prod-test-driver` has succeeded without errors.")
    else:
        logging.error(f"Execution of the `prod-test-driver` terminated with code={testrun_returncode}.")

    # In case of timeout error, we optionally send a slack notification.
    if testrun_returncode == TIMEOUT_CODE and is_slack_timeout_notify:
        slack_message = "\n".join(
            [
                f"Scheduled job `{CI_JOB_NAME}` *timed out*. <{CI_JOB_URL}|log>.",  # noqa
                f"Commit: <{CI_PROJECT_URL}/-/commit/{CI_COMMIT_SHA}|{CI_COMMIT_SHORT_SHA}>.",
                f"IC_VERSION_ID: `{IC_VERSION_ID}`.",  # noqa
            ]
        )
        returncode = notify_slack(slack_message, CI_PROJECT_DIR, SLACK_CHANNEL_NOTIFY)
        if returncode == 0:
            logging.info("Successfully sent timeout slack notification.")
        else:
            logging.error(f"Failed to send slack timeout notification, exit code={returncode}.")
    # Process all test result files produced by the test-driver execution and infer overall suite execution success/failure.
    process_results_cmd = (
        RUN_CMD
        + [PROCESS_TEST_RESULTS_SUBCOMMAND]
        + [
            f"--working-dir={working_dir}",
            f"--ci-job-url={CI_JOB_URL}",
            f"--ci-project-url={CI_PROJECT_URL}",
            f"--ci-commit-sha={CI_COMMIT_SHA}",
            f"--ci-commit-short-sha={CI_COMMIT_SHORT_SHA}",
            f"--ci-commit-date={commit_date}",
            f"--ic-version-id={IC_VERSION_ID}",
            f"--ic-version-id-date={ic_version_id_date}",
        ]
    )
    test_suite_returncode = run_command(command=process_results_cmd)
    # 0 - successful suite execution.
    # 1 - suite failed, case with some failed or interrupted tests.
    if not (test_suite_returncode == 0 or test_suite_returncode == 1):
        exit_with_log(f"Processing of the test results failed unexpectedly with code={test_suite_returncode}")

    if is_honeycomb_push:
        logging.info("Pushing results to honeycomb.")
        honeycomb_cmd = [
            "python3",
            f"{CI_PROJECT_DIR}/gitlab-ci/src/test_results/honeycomb.py",
            f"--test_results={RESULT_FILE}",
            f"--job_url={CI_JOB_URL}",
            f"--trace_id={ROOT_PIPELINE_ID}",
            f"--parent_id={JOB_ID}",
            f"--job_name={CI_JOB_NAME}",
            f"--ci_pipeline_source={CI_PIPELINE_SOURCE}",
            "--type=system-tests",
        ]
        honeycomb_returncode = run_command(command=honeycomb_cmd)
        if honeycomb_returncode == 0:
            logging.info("Successfully pushed results to honeycomb.")
        else:
            logging.error(f"{RED}Failed to push results to honeycomb.{NC}")

    if is_slack_test_failure_notify:
        filepath = os.path.join(working_dir, SLACK_FAILURE_ALERTS_FILE)
        send_all_slack_alerts(filepath, CI_PROJECT_DIR)

    logging.debug(f"SUMMARY_ARGS={SUMMARY_ARGS}")

    # NOTE: redirect stdout to stderr, to show the output in gitlab CI.
    # This hack should be reworked by importing the script and passing the logger.
    run_summary_cmd = [
        "python3",
        f"{CI_PROJECT_DIR}/gitlab-ci/src/test_results/summary.py",
    ] + SUMMARY_ARGS
    logging.debug(f"run_summary_cmd={run_summary_cmd}")
    summary_run_returncode = run_command(command=run_summary_cmd, env=env_dict, stdout=sys.stderr.fileno())
    if summary_run_returncode == 0:
        logging.info("Summary created successfully.")
    else:
        logging.error(f"{RED}Failed to create summary.{NC}")
    # For a better visibility of this log, it is placed closer to the end of the execution.
    if testrun_returncode == TIMEOUT_CODE:
        logging.info(
            f"{RED}Test suite execution has timed out after {SYSTEM_TESTS_TIMEOUT_SEC} sec. "
            f"Consider changing the SYSTEM_TESTS_TIMEOUT environment variable to a higher value."
            f"{NC}"
        )
        return TIMEOUT_CODE
    return test_suite_returncode


if __name__ == "__main__":
    # Check that for local runs script is launched from the nix-shell.
    is_local_run = os.getenv("CI_JOB_ID", default=None) is None
    in_nix_shell = "IN_NIX_SHELL" in os.environ
    if is_local_run and not in_nix_shell:
        exit_with_log("This script must be run from the nix-shell.")
    runner_args = sys.argv[1:]
    # on CI, we don't want to polute the job log with all the noisy test logs
    if not is_local_run:
        runner_args.append("--no-propagate-test-logs")
    logging.debug(f"Input arguments are: {runner_args}")
    if any([i in runner_args for i in ["-h", "--help"]]):
        run_help_command(SHELL_WRAPPER_DEFAULT)
        sys.exit(0)
    keep_tmp_artifacts_folder = False
    folders_to_remove: List[str] = []
    # Check if optional flag of keeping tmp artifact folder is set.
    if "--keep_artifacts" in runner_args:
        keep_tmp_artifacts_folder = True
        # Delete the flag from the arguments, as it is not intended for `prod-test-driver`
        runner_args.remove("--keep_artifacts")
    (working_dir_arg,) = try_extract_arguments(search_args=["--working-dir"], separator="=", args=runner_args)
    if not working_dir_arg:
        working_dir_arg = tempfile.mkdtemp(prefix="tmp_working_dir_")
        runner_args.append(f"--working-dir={working_dir_arg}")
        folders_to_remove.append(working_dir_arg)
    keep_tmp_dirs = False
    if "--keep_tmp_dirs" in runner_args:
        keep_tmp_dirs = True
        # Delete the flag from the arguments, as it is not intended for `prod-test-driver`
        runner_args.remove("--keep_tmp_dirs")
    logging.debug(f"runner_args arguments are: {runner_args}")
    # Run main() in the try/catch to delete tmp folders (marked for deletion) in case of exceptions or user interrupts.
    testrun_returncode = 1
    try:
        testrun_returncode = main(runner_args, working_dir_arg, folders_to_remove, keep_tmp_artifacts_folder)
    except Exception as e:
        logging.exception(f"Raised exception: {e}")
    finally:
        if not keep_tmp_dirs:
            remove_folders(folders_to_remove)
        if keep_tmp_artifacts_folder:
            logging.info(f"{RED}Artifacts folder is not deleted `--keep_artifacts` was set by the user.{NC}")
    sys.exit(testrun_returncode)

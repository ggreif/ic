#!/bin/bash
#
# the latest copy of this file can be downloaded from:
#    https://raw.githubusercontent.com/dfinity/ic/master/rs/backup/install.sh
# running the installation:
#    bash <(curl -L https://raw.githubusercontent.com/dfinity/ic/master/rs/backup/install.sh)
#
# required binaries on the backup pod: bash, curl, gunzip
#

TMP_DIR=$(mktemp -d)
BACKUP_EXE_NAME="ic-backup"
BACKUP_EXE="${TMP_DIR}/${BACKUP_EXE_NAME}"
BACKUP_EXE_GZ="${BACKUP_EXE}.gz"
CONFIG_FILE_NAME="config.json5"
CONFIG_FILE="${TMP_DIR}/${CONFIG_FILE_NAME}"
SERVICE_CONFIG_FILE="${TMP_DIR}/ic-backup.service"
UPDATE_FILE_NAME="update.sh"
UPDATE_FILE="${TMP_DIR}/${UPDATE_FILE_NAME}"
NNS_URL="https://ic0.app"
PUBLIC_KEY_NAME="ic_public_key.pem"
PUBLIC_KEY_FILE="${TMP_DIR}/${PUBLIC_KEY_NAME}"
NODES_SYNCING=5
SYNCING_PERIOD=1800 # 1/2 hour
REPLAY_PERIOD=14400 # 4 hours
BACKUP_INSTANCE=$(hostname -a)

DEFAULT_BUILD_ID="f0c69aebc64fc0ec52f1579d5ed049006a70e050"
echo "Enter the BUILD_ID of the proper ic-backup version:"
echo "(default: ${DEFAULT_BUILD_ID}):"
read BUILD_ID
if [ -z "${BUILD_ID// /}" ]; then
    BUILD_ID=${DEFAULT_BUILD_ID}
fi

DEFAULT_USER_ID=$(whoami)
echo "Enter the local USER_ID that will run the backup:"
echo "(default: ${DEFAULT_USER_ID}):"
read USER_ID
if [ -z "${USER_ID// /}" ]; then
    USER_ID=${DEFAULT_USER_ID}
fi

BACKUP_HOME=$(grep "^${USER_ID}:" /etc/passwd | cut -d ":" -f6)
if [ -z "${BACKUP_HOME// /}" ]; then
    echo "Error: User ${USER_ID} is not found!"
    exit 1
fi

DEFAULT_WORK_DIR="/var/backups/ic"
echo "Please enter backup work directory"
echo "(default: ${DEFAULT_WORK_DIR}):"
read INPUT_WORK_DIR
if [ -z "${INPUT_WORK_DIR// /}" ]; then
    INPUT_WORK_DIR=${DEFAULT_WORK_DIR}
fi

WORK_DIR=$(realpath -s ${INPUT_WORK_DIR})

echo
echo
echo "BUILD_ID: '${BUILD_ID}'"
echo "WORK_DIR: '${WORK_DIR}'"
echo
echo

ROOT_DIR="${WORK_DIR}/backup"

DOWNLOAD_URL="https://download.dfinity.systems/ic/${BUILD_ID}/release/${BACKUP_EXE_NAME}.gz"
echo "Downloading: ${DOWNLOAD_URL}"
curl -L ${DOWNLOAD_URL} --output ${BACKUP_EXE_GZ}

gunzip ${BACKUP_EXE_GZ}
chmod +x ${BACKUP_EXE}

read -r -d '' CONFIG <<-EOM
{
    "version": 2,
    "push_metrics": true,
    "backup_instance": "${BACKUP_INSTANCE}",
    "nns_url": "${NNS_URL}",
    "nns_pem": "${WORK_DIR}/$PUBLIC_KEY_NAME",
    "root_dir": "${ROOT_DIR}",
    "excluded_dirs": [
        "backups", 
        "diverged_checkpoints", 
        "diverged_state_markers",
        "fs_tmp", 
        "tip", 
        "tmp"
    ],
    "ssh_private_key": "${BACKUP_HOME}/.ssh/id_ed25519_backup",
    "disk_threshold_warn": 75,
    "slack_token": "<INSERT SLACK TOKEN>",
    "subnets": [
        {
            "subnet_id": "<INSERT 1. SUBNET_ID>",
            "initial_replica_version": "<INSERT REPLICA_VERSION>",
            "nodes_syncing": ${NODES_SYNCING},
            "sync_period_secs": ${SYNCING_PERIOD},
            "replay_period_secs": ${REPLAY_PERIOD}
        },
        {
            "subnet_id": "<INSERT 2. SUBNET_ID>",
            "initial_replica_version": "<INSERT REPLICA_VERSION>",
            "nodes_syncing": ${NODES_SYNCING},
            "sync_period_secs": ${SYNCING_PERIOD},
            "replay_period_secs": ${REPLAY_PERIOD}
        }
    ]
}
EOM

echo "${CONFIG}" >${CONFIG_FILE}

PUBLIC_KEY="-----BEGIN PUBLIC KEY-----
MIGCMB0GDSsGAQQBgtx8BQMBAgEGDCsGAQQBgtx8BQMCAQNhAIFMDm7HH6tYOwi9
gTc8JVw8NxsuhIY8mKTx4It0I10U+12cDNVG2WhfkToMCyzFNBWDv0tDkuRn25bW
W5u0y3FxEvhHLg1aTRRQX/10hLASkQkcX4e5iINGP5gJGguqrg==
-----END PUBLIC KEY-----"

echo "${PUBLIC_KEY}" >${PUBLIC_KEY_FILE}

SERVICE_CONFIG="
[Unit]
Description=IC backup
After=systemd-networkd.service

[Service]
Type=simple
User=${USER_ID}
Environment=RUST_MIN_STACK=8192000
WorkingDirectory=${WORK_DIR}
ExecStart=${WORK_DIR}/ic-backup --config-file ${WORK_DIR}/config.json5
Restart=always

[Install]
WantedBy=multi-user.target
"

echo "${SERVICE_CONFIG}" >${SERVICE_CONFIG_FILE}

UPDATE_CONTENT="#!/bin/bash
bash <(curl -L https://raw.githubusercontent.com/dfinity/ic/master/rs/backup/upgrade.sh)
"

echo "${UPDATE_CONTENT}" >${UPDATE_FILE}
chmod +x ${UPDATE_FILE}

mkdir -p ${WORK_DIR}
mkdir -p ${ROOT_DIR}
cp ${BACKUP_EXE} ${WORK_DIR}
cp ${CONFIG_FILE} ${WORK_DIR}
cp ${PUBLIC_KEY_FILE} ${WORK_DIR}
cp ${UPDATE_FILE} ${WORK_DIR}

echo "Installing system config..."
sudo cp ${SERVICE_CONFIG_FILE} /etc/systemd/system
echo "Reloading services..."
sudo systemctl daemon-reload

rm -rf ${TMP_DIR}

echo
echo
echo
echo "Please edit the config file ${CONFIG_FILE_NAME} placed in ${WORK_DIR}"
echo
echo "then initialise subnet backups with an existing execution state by running this command:"
echo "${WORK_DIR}/ic-backup --config-file ${WORK_DIR}/config.json5 init"
echo
echo "finaly start the backup service with this command:"
echo "sudo systemctl start ic-backup.service"
echo
echo "also consider to let it run on a reboot with this command:"
echo "sudo systemctl enable ic-backup.service"
echo
echo "Later on you can periodically update to the latest version of the backup tool"
echo "by running ${UPDATE_FILE_NAME} in the ${WORK_DIR}"
echo

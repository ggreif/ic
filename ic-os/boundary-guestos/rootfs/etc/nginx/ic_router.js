import qs from "querystring";

import SUBNET_TABLE from "/var/opt/nginx/ic/ic_routes.js";
import CANISTER_ID_ALIASES from "/var/opt/nginx/canister_aliases/canister_id_aliases.js";

const CANISTER_ID_LENGTH = 27;

function leftpad(s, len, pad) {
  return (
    len + 1 >= s.length && (s = new Array(len + 1 - s.length).join(pad) + s), s
  );
}

function decode_canister_id(canister_id) {
  canister_id = canister_id.replace(/-/g, "");
  var RFC4628 = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  var hex = "";
  var bits = "";
  for (var i = 0; i < canister_id.length; i++) {
    var val = RFC4628.indexOf(canister_id.charAt(i).toUpperCase());
    bits += leftpad(val.toString(2), 5, "0");
  }
  for (i = 32; i + 4 <= bits.length; i += 4) {
    var chunk = bits.substr(i, 4);
    hex += parseInt(chunk, 2).toString(16);
  }
  return hex;
}

// Find the first row before the given canister_id.
function find_subnet(canister_id, table) {
  var start = 0;
  var end = table.canister_range_starts.length - 1;
  while (start <= end) {
    var mid = Math.floor((start + end) / 2);
    var mid_value = table.canister_range_starts[mid];
    if (mid_value >= canister_id) {
      end = mid - 1;
    } else {
      start = mid + 1;
    }
  }
  if (start > 0 && canister_id < table.canister_range_starts[start]) {
    return start - 1;
  } else {
    return Math.min(start, table.canister_range_starts.length - 1);
  }
}

function resolve_canister_id_from_uri(uri) {
  var re = /^\/api\/v2\/canister\/([0-9a-z\-]+)\//;
  var m = re.exec(uri);
  if (!m) {
    return "";
  }
  var canister_id = m[1];
  if (canister_id.length != CANISTER_ID_LENGTH) {
    // not a canister id
    return "";
  }
  return canister_id;
}

function extractCanisterIdFromHost(host) {
  let re = /^([0-9a-zA-Z\-]+)\./;
  let m = re.exec(host);
  if (!m) {
    return "";
  }
  let canisterId = m[1];

  // Check if ID is an alias
  if (!!CANISTER_ID_ALIASES[canisterId]) {
    canisterId = CANISTER_ID_ALIASES[canisterId];
  }

  if (canisterId.length != CANISTER_ID_LENGTH) {
    return "";
  }

  return canisterId;
}

function resolve_ci_from_host(host) {
  var pieces = host.split(".");
  if (pieces.length < 3) {
    return "";
  }
  var ic = pieces[pieces.length - 3];
  if (ic.length >= 27) {
    // This is a canister_id.
    return "";
  }
  return ic;
}

function get_hostname_from_uri(uri) {
  var re = /^https?\:\/\/([^:\/?#]*)/;
  var m = re.exec(uri);
  if (!m) {
    return "";
  }
  return m[1];
}

function extractCanisterIdFromReferer(r) {
  var refererHeader = r.headersIn.referer;
  if (!refererHeader) {
    return "";
  }

  var refererHost = get_hostname_from_uri(refererHeader);
  if (!refererHost) {
    return "";
  }

  canisterId = extractCanisterIdFromHost(refererHost);
  if (!!canisterId) {
    return canisterId;
  }

  var idx = refererHeader.indexOf("?");
  if (i != -1) {
    return "";
  }

  var queryParams = qs.parse(refererHeader.substr(idx + 1));
  return queryParams["canisterId"];
}

function hostCanisterId(r) {
  return extractCanisterIdFromHost(r.headersIn.host);
}

function inferCanisterId(r) {
  // URI
  var canisterId = resolve_canister_id_from_uri(r.uri);
  if (!!canisterId) {
    return canisterId;
  }

  // Host
  var canisterId = extractCanisterIdFromHost(r.headersIn.host);
  if (!!canisterId) {
    return canisterId;
  }

  // Query param
  var canisterId = r.args["canisterId"];
  if (!!canisterId) {
    return canisterId;
  }

  // Referer
  return extractCanisterIdFromReferer(r);
}

function isTableEmpty(r) {
  return !SUBNET_TABLE["canister_subnets"] ? "1" : "";
}

function normalizeSubnetType(typ) {
  return (
    {
      application: "application",
      system: "system",
    }[typ] || ""
  );
}

function route(r) {
  var canister_id = inferCanisterId(r);
  if (!canister_id) {
    return "";
  }

  // TODO: Lookup custom domain via ci
  // if (!canister_id && ci) {
  //   var custom_route = lookup_custom_route(ci);
  //   if (custom_route) {
  //     canister_id = custom_route.canister_id;
  //     ic = custom_route.ic;
  //   }
  // }
  if (!("canister_subnets" in SUBNET_TABLE)) {
    return "";
  }

  canister_id = decode_canister_id(canister_id);
  var subnet_index = find_subnet(canister_id, SUBNET_TABLE);
  if (
    canister_id < SUBNET_TABLE.canister_range_starts[subnet_index] ||
    canister_id > SUBNET_TABLE.canister_range_ends[subnet_index]
  ) {
    return "";
  }

  var subnet_id = SUBNET_TABLE.canister_subnets[subnet_index];

  var subnet_types = SUBNET_TABLE["subnet_types"] || {};
  var subnet_type = normalizeSubnetType(subnet_types[subnet_id]);

  var nodes = SUBNET_TABLE.subnet_nodes[subnet_id];
  if (nodes.length < 1) {
    return "";
  }

  var node_index = Math.floor(Math.random() * Math.floor(nodes.length));
  var node_ids = SUBNET_TABLE.subnet_node_ids[subnet_id];
  var node_id = node_ids[node_index];
  r.headersOut["x-ic-subnet-id"] = subnet_id;
  r.headersOut["x-ic-node-id"] = node_id;
  if (canister_id) {
    r.headersOut["x-ic-canister-id"] = canister_id;
  }

  return `${node_id},${subnet_id},${subnet_type}`;
}

function randomRoute() {
  var canisterSubnets = SUBNET_TABLE.canister_subnets || [];
  var subnetCount = canisterSubnets.length;
  if (subnetCount == 0) {
    return "";
  }

  // Choose random subnet
  var subnetIdx = Math.floor(Math.random() * subnetCount);
  var subnetId = canisterSubnets[subnetIdx];

  var subnetNodeIds = SUBNET_TABLE.subnet_node_ids[subnetId] || [];
  var nodeCount = subnetNodeIds.length;
  if (nodeCount == 0) {
    return "";
  }

  var subnetTypes = SUBNET_TABLE["subnet_types"] || {};
  var subnetType = normalizeSubnetType(subnetTypes[subnetId]);

  // Choose random node
  var nodeIdx = Math.floor(Math.random() * nodeCount);
  var nodeId = subnetNodeIds[nodeIdx];

  return `${subnetId},${subnetType},${nodeId}`;
}

export default {
  hostCanisterId,
  inferCanisterId,
  isTableEmpty,
  randomRoute,
  route,
};

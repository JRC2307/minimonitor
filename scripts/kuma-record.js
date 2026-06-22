// One-shot socket.io frame recorder for Uptime-Kuma 1.23.16.
// Runs INSIDE the kuma-fix container (it bundles socket.io-client 4.8.1).
// Completes first-run setup, logs in, captures the pushed monitorList, then
// add/edit/delete — dumping each ACK / broadcast as JSON to stdout under
// clearly-delimited markers the host harness slices into fixture files.
//
// Usage (from host):
//   docker cp scripts/kuma-record.js kuma-fix:/tmp/rec.js
//   docker exec kuma-fix node /tmp/rec.js
const { io } = require("socket.io-client");

const URL = "http://127.0.0.1:3001";
const USER = "admin";
const PASS = "fleet-test-pw-123";

function dump(label, obj) {
  console.log(`===BEGIN ${label}===`);
  console.log(JSON.stringify(obj, null, 2));
  console.log(`===END ${label}===`);
}

function emitAck(sock, event, payload, timeoutMs = 10000) {
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error(`ack timeout: ${event}`)), timeoutMs);
    sock.emit(event, payload, (res) => {
      clearTimeout(t);
      resolve(res);
    });
  });
}

// The Kuma `setup` handler is (username, password, callback) — positional.
function emitSetupAck(sock, username, password, timeoutMs = 10000) {
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error("ack timeout: setup")), timeoutMs);
    sock.emit("setup", username, password, (res) => {
      clearTimeout(t);
      resolve(res);
    });
  });
}

(async () => {
  let monitorListResolve;
  const monitorListPromise = new Promise((r) => (monitorListResolve = r));
  let firstMonitorList = null;

  const sock = io(URL, { transports: ["websocket"], reconnection: false });

  // Arm the monitorList handler BEFORE connect (the push-based dance).
  let lastMonitorList = null;
  sock.on("monitorList", (list) => {
    lastMonitorList = list;
    if (firstMonitorList === null) {
      firstMonitorList = list;
      monitorListResolve(list);
    }
  });

  sock.on("connect_error", (e) => {
    console.error("connect_error", e.message);
  });

  await new Promise((r, rej) => {
    sock.on("connect", r);
    sock.on("connect_error", rej);
  });
  console.error("connected, sid=" + sock.id);

  // First-run setup: create the admin user. If already set up, this errors
  // (ok=false) and we proceed to login.
  try {
    const setupRes = await emitSetupAck(sock, USER, PASS);
    console.error("setup:", JSON.stringify(setupRes));
  } catch (e) {
    console.error("setup skipped:", e.message);
  }

  // Login — the ACK carries the JWT token.
  const loginAck = await emitAck(sock, "login", {
    username: USER,
    password: PASS,
    token: "",
  });
  dump("LOGIN_ACK", loginAck);

  // The monitorList broadcast is pushed on auth; wait for it (pre-armed oneshot).
  const list = await Promise.race([
    monitorListPromise,
    new Promise((_, rej) => setTimeout(() => rej(new Error("no monitorList push")), 8000)),
  ]).catch((e) => {
    console.error(e.message);
    return firstMonitorList || {};
  });
  dump("MONITOR_LIST_INITIAL", list);

  // The exact MonitorSpec shape we serialize in Rust.
  const spec = {
    type: "ping",
    name: "nas-01",
    hostname: "100.64.0.1",
    interval: 60,
    maxretries: 1,
    retryInterval: 60,
    resendInterval: 0,
    upsideDown: false,
    accepted_statuscodes: ["200-299"],
    kafkaProducerBrokers: [],
    kafkaProducerSaslOptions: {},
    notificationIDList: {},
  };

  // Create an ntfy notification first, so notificationIDList:{<id>:true} is real.
  let ntfyId = null;
  try {
    const notifAck = await new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error("ack timeout: addNotification")), 10000);
      sock.emit(
        "addNotification",
        {
          name: "fleet-ntfy",
          type: "ntfy",
          ntfyserverurl: "https://ntfy.sh",
          ntfytopic: "fleet",
          ntfyPriority: 5,
          ntfyAuthenticationMethod: "none",
        },
        null,
        (res) => {
          clearTimeout(t);
          resolve(res);
        },
      );
    });
    dump("ADD_NOTIFICATION_ACK", notifAck);
    ntfyId = notifAck && notifAck.id;
    console.error("ntfyId=" + ntfyId);
  } catch (e) {
    console.error("addNotification failed:", e.message);
  }
  if (ntfyId != null) {
    spec.notificationIDList = { [ntfyId]: true };
  }

  // ADD a ping monitor.
  const addAck = await emitAck(sock, "add", spec);
  dump("ADD_ACK", addAck);
  const monitorID = addAck && addAck.monitorID;
  console.error("new monitorID=" + monitorID);

  // Capture the full server-side monitor object after add (the version-pinned
  // shape, for the contract fixture / RemoteMonitor parsing).
  let fullMonitor = null;
  try {
    const getAck = await emitAck(sock, "getMonitor", monitorID);
    dump("GET_MONITOR", getAck);
    fullMonitor = getAck;
  } catch (e) {
    console.error("getMonitor failed:", e.message);
  }

  // Capture the monitorList broadcast that `add` pushed (non-empty — keyed by id).
  await new Promise((r) => setTimeout(r, 500));
  dump("MONITOR_LIST_AFTER_ADD", lastMonitorList || {});

  // EDIT — send the FULL object back with a drifted interval.
  const editSpec = Object.assign({}, (fullMonitor && fullMonitor.monitor) || spec, {
    id: monitorID,
    interval: 120,
  });
  const editAck = await emitAck(sock, "editMonitor", editSpec);
  dump("EDIT_ACK", editAck);

  // DELETE.
  const deleteAck = await emitAck(sock, "deleteMonitor", monitorID);
  dump("DELETE_ACK", deleteAck);

  sock.close();
  console.error("done");
  process.exit(0);
})().catch((e) => {
  console.error("FATAL", e);
  process.exit(1);
});

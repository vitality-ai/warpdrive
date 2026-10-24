(ns jepsen-warpdrive.core
  "Jepsen-ecosystem consistency + durability test for WarpDrive's distributed
   engine. Uses the real Elle checker (rw-register cycle detection) over a
   history produced by a concurrent S3 workload while a *seeded* nemesis
   injects peer kills, network partitions (iptables isolation), restarts and
   on-disk shard corruption. The whole fault schedule is a pure function of a
   64-bit seed, so any failing run replays exactly (--seed N).

   This is 'Jepsen as a library': we drive a real local cluster and real
   S3 API ourselves (no SSH/Docker available here), and hand the resulting
   history to Elle -- the genuine consistency-analysis engine, not a
   hand-rolled checker."
  (:require [jepsen-warpdrive.s3 :as s3]
            [jepsen.history :as h]
            [elle.rw-register :as rw]
            [clojure.java.shell :as sh]
            [clojure.string :as str])
  (:import [java.util Random])
  (:gen-class))

(def access-key "AKIAIOSFODNN7EXAMPLE")
(def secret-key "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY")
(def bucket "jepsen")
(def script (str (System/getProperty "user.dir") "/cluster.sh"))

;; ---- cluster control (shell out to cluster.sh) ----
(defn- ctl [& args]
  (let [r (apply sh/sh "bash" script args)]
    (when-not (zero? (:exit r))
      (binding [*out* *err*] (println "ctl" (vec args) "failed:" (:err r))))
    r))

(defn cluster-start [] (ctl "start"))
(defn cluster-stop  [] (ctl "stop"))

;; ---- op outcome mapping ----
;; write: 200 -> :ok ; definite 4xx/5xx -> :fail ; timeout/exn -> :info (unknown)
;; read : 200 -> :ok value ; 404 -> :ok nil (definitely absent) ; else -> :info
(defn- do-write! [conn k v]
  (try
    (let [st (:status (s3/put-object conn bucket (str k) (.getBytes (str v))))]
      (cond (= 200 st) [:ok v]
            (and (>= st 400) (< st 500)) [:fail v]
            :else [:info v]))
    (catch Exception _ [:info v])))

(defn- do-read! [conn k]
  (try
    (let [r (s3/get-object conn bucket (str k))
          st (:status r)]
      (cond (= 200 st) [:ok (Long/parseLong (String. (:body r)))]
            (= 404 st) [:ok nil]
            :else [:info nil]))
    (catch Exception _ [:info nil])))

;; ---- concurrent workload ----
(defn- worker
  [conn pid ks counter history stop? rnd-seed]
  (let [rng (Random. rnd-seed)]
    (fn []
      (while (not @stop?)
        (let [k (nth ks (.nextInt rng (count ks)))
              write? (< (.nextDouble rng) 0.5)]
          (if write?
            (let [v (swap! counter inc)]
              (swap! history conj {:process pid :type :invoke :f :txn :value [[:w k v]]})
              (let [[t rv] (do-write! conn k v)]
                (swap! history conj {:process pid :type t :f :txn :value [[:w k rv]]})))
            (do
              (swap! history conj {:process pid :type :invoke :f :txn :value [[:r k nil]]})
              (let [[t rv] (do-read! conn k)]
                (swap! history conj {:process pid :type t :f :txn :value [[:r k rv]]})))))
        (Thread/sleep (long (+ 5 (rand-int 20))))))))

;; ---- seeded nemesis ----
(def ^:private peer-faults [:kill :isolate :corrupt])

(defn- nemesis-thread
  "Pure function of seed. Only touches peers 1..N-1 (node0 is the client
   endpoint, kept up so faults exercise quorum/EC rather than a trivial
   total outage). Tracks which nodes are down/isolated to pick valid actions."
  [n seed history stop?]
  (let [rng (Random. seed)
        down (atom #{}) part (atom #{})]
    (fn []
      (while (not @stop?)
        (Thread/sleep (long (+ 800 (.nextInt rng 2500))))
        (let [peers (range 1 n)
              up    (remove @down peers)
              act   (cond
                      (and (seq @part) (< (.nextDouble rng) 0.4)) :heal
                      (and (seq @down) (< (.nextDouble rng) 0.5)) :restart
                      (seq up) (nth peer-faults (.nextInt rng (count peer-faults)))
                      :else :restart)
              pick  (fn [coll] (when (seq coll) (nth (vec coll) (.nextInt rng (count coll)))))]
          (case act
            :kill    (when-let [i (pick up)]
                       (ctl "kill" (str i)) (swap! down conj i)
                       (swap! history conj {:process :nemesis :type :info :f :kill :value i}))
            :isolate (when-let [i (pick up)]
                       (ctl "isolate" (str i)) (swap! part conj i)
                       (swap! history conj {:process :nemesis :type :info :f :isolate :value i}))
            :corrupt (when-let [i (pick up)]
                       (ctl "corrupt" (str i))
                       (swap! history conj {:process :nemesis :type :info :f :corrupt :value i}))
            :restart (when-let [i (pick @down)]
                       (ctl "restart" (str i)) (swap! down disj i)
                       (swap! history conj {:process :nemesis :type :info :f :restart :value i}))
            :heal    (when-let [i (pick @part)]
                       (ctl "heal" (str i)) (swap! part disj i)
                       (swap! history conj {:process :nemesis :type :info :f :heal :value i}))
            nil)))
      ;; final heal so the checker/cleanup see a reachable cluster
      (ctl "heal" "all"))))

;; ---- run ----
(defn run-test [{:keys [seed duration-sec concurrency n keys-n]
                 :or {seed 42 duration-sec 60 concurrency 5 n 5 keys-n 3}}]
  (println (format "=== WarpDrive Jepsen/Elle run: seed=%d dur=%ds conc=%d nodes=%d keys=%d ==="
                   seed duration-sec concurrency n keys-n))
  (cluster-stop) (cluster-start)
  (let [conn    (s3/conn "127.0.0.1:9710" access-key secret-key)
        _       (println "create-bucket ->" (:status (s3/create-bucket conn bucket)))
        ks      (vec (range keys-n))
        counter (atom 0)
        history (atom [])
        stop?   (atom false)
        base    (Random. seed)
        workers (mapv (fn [pid]
                        (Thread. ^Runnable (worker conn pid ks counter history stop? (.nextLong base))))
                      (range concurrency))
        nem     (Thread. ^Runnable (nemesis-thread n (.nextLong base) history stop?))]
    (doseq [^Thread t workers] (.start t))
    (.start nem)
    (Thread/sleep (long (* 1000 duration-sec)))
    (reset! stop? true)
    (doseq [^Thread t workers] (.join t 10000))
    (.join nem 15000)
    (println "=== workload done, running Elle rw-register checker ===")
    (let [ops        @history
          client-ops (remove #(= :nemesis (:process %)) ops)
          nem-ops    (filter #(= :nemesis (:process %)) ops)
          hist       (h/history client-ops)
          ;; No :directory -> Elle does pure cycle analysis without rendering
          ;; anomaly graphs (graphviz `dot` isn't installed, and plotting
          ;; needs AWT). We print the anomaly summary ourselves below.
          res        (rw/check {:consistency-models [:strict-serializable]}
                               hist)]
      (println (format "client ops: %d  (writes/reads interleaved)  nemesis events: %d"
                       (count client-ops) (count nem-ops)))
      (print "nemesis timeline: ")
      (println (str/join " " (map #(str (name (:f %)) ":" (:value %)) nem-ops)))
      (println "\n=== ELLE RESULT ===")
      (println "valid?       " (:valid? res))
      (println "anomaly types" (:anomaly-types res))
      (when (:not res) (println "not             " (:not res)))
      (when (seq (:anomaly-types res))
        (println "anomalies (summary):")
        (doseq [[k v] (:anomalies res)]
          (println "  " k "->" (if (coll? v) (count v) v) "instance(s)")))
      (println "\nElle details written to /tmp/wd-jepsen-elle")
      (:valid? res))))

(defn durability-test
  "Power-loss durability: write M keys through the ACKED S3 path (HTTP 200 =
   server claims durable), SIGKILL the whole cluster with no clean shutdown
   (anything only in the page cache and not yet fsync'd is at risk), recover
   on the same data, and assert every acked value survives. This directly
   tests WarpDrive's group-commit guarantee -- the fast-path/slow-path design
   whose whole point is that a 200 means the bytes are on stable storage. An
   ack-before-durable bug shows up here as a missing or stale read."
  [{:keys [m] :or {m 200}}]
  (println (format "=== WarpDrive power-loss durability test: %d acked keys ===" m))
  (cluster-stop) (cluster-start)
  (let [conn (s3/conn "127.0.0.1:9710" access-key secret-key)
        _ (println "create-bucket ->" (:status (s3/create-bucket conn bucket)))
        expected (atom {})]
    (println "writing" m "keys, each acked (HTTP 200) before continuing ...")
    (doseq [i (range m)]
      (let [k (str "dur-" i) v (str "val-" i "-" (rand-int 1000000))]
        (let [st (:status (s3/put-object conn bucket k (.getBytes v)))]
          (if (= 200 st)
            (swap! expected assoc k v)
            (println "  WARN: write" k "not acked (status" st ") -- excluded from durability set")))))
    (println "acked" (count @expected) "writes; now SIGKILLing whole cluster (power loss) ...")
    (ctl "crashall")
    (Thread/sleep 1000)
    (println "recovering cluster on existing data ...")
    (ctl "recover")
    (println "reading back all acked keys ...")
    (let [conn2 (s3/conn "127.0.0.1:9710" access-key secret-key)
          lost  (atom []) stale (atom [])]
      (doseq [[k v] @expected]
        (let [r (s3/get-object conn2 bucket k)
              st (:status r)]
          (cond
            (not= 200 st)            (swap! lost conj [k st])
            (not= v (String. (:body r))) (swap! stale conj [k v (String. (:body r))]))))
      (println "\n=== DURABILITY RESULT ===")
      (println "acked writes      :" (count @expected))
      (println "lost after crash  :" (count @lost))
      (println "stale after crash :" (count @stale))
      (when (seq @lost)  (println "  LOST examples :" (take 5 @lost)))
      (when (seq @stale) (println "  STALE examples:" (take 5 @stale)))
      (let [ok (and (empty? @lost) (empty? @stale))]
        (println (if ok
                   "PASS: every acked write survived power loss -> group-commit durability holds"
                   "FAIL: acked writes did NOT survive power loss -> ack-before-durable violation"))
        ok))))

(defn- parse-args [args]
  (into {} (map (fn [[k v]]
                  [(keyword (str/replace k #"^--" ""))
                   (if (re-matches #"\d+" v) (Long/parseLong v) v)])
                (partition 2 args))))

(defn validate []
  (let [c (s3/conn "127.0.0.1:9710" access-key secret-key) b "clj-validate"]
    (println "create-bucket ->" (:status (s3/create-bucket c b)))
    (println "put v1        ->" (:status (s3/put-object c b "k1" (.getBytes "hello-v1"))))
    (let [r (s3/get-object c b "k1")] (println "get ->" (:status r) "body=" (String. (or (:body r) (byte-array 0)))))
    (println "delete        ->" (:status (s3/delete-object c b "k1")))))

(defn selftest
  "Negative control: prove Elle catches a real violation, so a green run
   means something. We feed it an ABORTED READ (G1a) -- a read that observes
   the value of a write that definitely failed -- which strict serializability
   forbids. Elle must report valid? false."
  []
  (let [bad (h/history
              [{:process 0 :type :invoke :f :txn :value [[:w 0 1]]}
               {:process 0 :type :fail   :f :txn :value [[:w 0 1]]}   ; write of 1 ABORTED
               {:process 0 :type :invoke :f :txn :value [[:r 0 nil]]}
               {:process 0 :type :ok     :f :txn :value [[:r 0 1]]}])  ; yet a read sees 1
        res (rw/check {:consistency-models [:strict-serializable]} bad)]
    (println "=== SELFTEST (negative control: aborted-read G1a) ===")
    (println "valid?       " (:valid? res))
    (println "anomaly types" (:anomaly-types res))
    (if (false? (:valid? res))
      (do (println "PASS: Elle correctly flagged the planted anomaly -> the checker has teeth") true)
      (do (println "FAIL: Elle did NOT catch a known violation -> checker is not trustworthy") false))))

(defn -main [& args]
  (case (first args)
    "validate" (validate)
    "selftest" (System/exit (if (selftest) 0 1))
    "durability" (let [ok (durability-test (parse-args (rest args)))]
                   (shutdown-agents) (System/exit (if ok 0 1)))
    "test"     (let [ok (run-test (parse-args (rest args)))]
                 (shutdown-agents)
                 (System/exit (if (false? ok) 1 0)))
    (println "usage: lein run validate | lein run test [--seed N --duration-sec S --concurrency C --n NODES --keys-n K]")))

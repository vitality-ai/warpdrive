(ns jepsen-warpdrive.s3
  "Minimal S3 client with AWS SigV4 signing, pointed at WarpDrive's
   authenticated cluster surface (/cluster/s3). We sign exactly the three
   headers boto3 signs for these ops (host;x-amz-content-sha256;x-amz-date),
   which is what WarpDrive's auth::authenticate_s3_request verifies. This is
   the same real API path the Python boto3 harness uses, so the consistency
   test exercises the real client-facing write path, not an internal one."
  (:require [clj-http.client :as http])
  (:import [javax.crypto Mac]
           [javax.crypto.spec SecretKeySpec]
           [java.security MessageDigest]
           [java.time ZonedDateTime ZoneOffset]
           [java.time.format DateTimeFormatter]
           [java.util Locale]))

(def ^:private region "us-east-1")
(def ^:private service "s3")

(defn- sha256-hex ^String [^bytes b]
  (let [md (MessageDigest/getInstance "SHA-256")]
    (apply str (map #(format "%02x" (bit-and % 0xff)) (.digest md b)))))

(defn- hmac ^bytes [^bytes k ^String data]
  (let [mac (Mac/getInstance "HmacSHA256")]
    (.init mac (SecretKeySpec. k "HmacSHA256"))
    (.doFinal mac (.getBytes data "UTF-8"))))

(defn- signing-key ^bytes [secret datestamp]
  (-> (hmac (.getBytes (str "AWS4" secret) "UTF-8") datestamp)
      (hmac region)
      (hmac service)
      (hmac "aws4_request")))

(defn- amz-dates []
  (let [now (ZonedDateTime/now ZoneOffset/UTC)]
    {:amzdate   (.format now (DateTimeFormatter/ofPattern "yyyyMMdd'T'HHmmss'Z'" Locale/US))
     :datestamp (.format now (DateTimeFormatter/ofPattern "yyyyMMdd" Locale/US))}))

(defn- signed
  "Build signed headers for one request. `path` is the full canonical URI
   (e.g. /cluster/s3/bucket/key). `body` is a byte array (possibly empty)."
  [{:keys [access-key secret-key host]} method path ^bytes body]
  (let [{:keys [amzdate datestamp]} (amz-dates)
        payload-hash (sha256-hex body)
        canonical-headers (str "host:" host "\n"
                               "x-amz-content-sha256:" payload-hash "\n"
                               "x-amz-date:" amzdate "\n")
        signed-headers "host;x-amz-content-sha256;x-amz-date"
        canonical-request (str method "\n" path "\n" ""  "\n"
                               canonical-headers "\n"
                               signed-headers "\n"
                               payload-hash)
        scope (str datestamp "/" region "/" service "/aws4_request")
        string-to-sign (str "AWS4-HMAC-SHA256\n" amzdate "\n" scope "\n"
                            (sha256-hex (.getBytes canonical-request "UTF-8")))
        sig (apply str (map #(format "%02x" (bit-and % 0xff))
                            (hmac (signing-key secret-key datestamp) string-to-sign)))
        auth (str "AWS4-HMAC-SHA256 Credential=" access-key "/" scope
                  ", SignedHeaders=" signed-headers ", Signature=" sig)]
    {"Authorization" auth
     "x-amz-date" amzdate
     "x-amz-content-sha256" payload-hash
     "Host" host}))

(defn- base-url [{:keys [host]}] (str "http://" host "/cluster/s3"))

(defn create-bucket [conn bucket]
  (let [path (str "/cluster/s3/" bucket)]
    (http/put (str (base-url conn) "/" bucket)
              {:headers (signed conn "PUT" path (byte-array 0))
               :throw-exceptions false :socket-timeout 5000 :connection-timeout 3000})))

(defn put-object [conn bucket k ^bytes body]
  (let [path (str "/cluster/s3/" bucket "/" k)]
    (http/put (str (base-url conn) "/" bucket "/" k)
              {:headers (signed conn "PUT" path body)
               :body body
               :throw-exceptions false :socket-timeout 5000 :connection-timeout 3000})))

(defn get-object [conn bucket k]
  (let [path (str "/cluster/s3/" bucket "/" k)]
    (http/get (str (base-url conn) "/" bucket "/" k)
              {:headers (signed conn "GET" path (byte-array 0))
               :as :byte-array
               :throw-exceptions false :socket-timeout 5000 :connection-timeout 3000})))

(defn delete-object [conn bucket k]
  (let [path (str "/cluster/s3/" bucket "/" k)]
    (http/delete (str (base-url conn) "/" bucket "/" k)
                 {:headers (signed conn "DELETE" path (byte-array 0))
                  :throw-exceptions false :socket-timeout 5000 :connection-timeout 3000})))

(defn conn [host access-key secret-key]
  {:host host :access-key access-key :secret-key secret-key})

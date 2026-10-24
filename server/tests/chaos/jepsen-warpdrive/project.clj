(defproject jepsen-warpdrive "0.1.0-SNAPSHOT"
  :description "Jepsen + Elle consistency/durability test harness for WarpDrive"
  :dependencies [[org.clojure/clojure "1.11.3"]
                 [jepsen "0.3.7"]
                 [elle "0.2.4"]
                 [clj-http "3.13.0"]
                 [cheshire "5.13.0"]]
  :main jepsen-warpdrive.core
  :jvm-opts ["-Djdk.attach.allowAttachSelf" "-Djava.awt.headless=true"])

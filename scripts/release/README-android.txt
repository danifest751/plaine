plaine-miner for Android (arm64-v8a, Android 7 and newer) - {VERSION}
https://github.com/danifest751/plaine

Run it from a shell on the phone, for example through adb from a computer:

  adb push plaine-miner /data/local/tmp/
  adb shell chmod 755 /data/local/tmp/plaine-miner
  adb shell /data/local/tmp/plaine-miner plne1youraddress.phone@eu.rplant.xyz:17190

It uses the phone's fastest cores first. A Poco X3 Pro (Snapdragon 860) mines about
5 kH/s. At full load a phone gets hot and drains its battery: keep it cool and charging.

--help lists every option; --bench measures the phone without a pool.

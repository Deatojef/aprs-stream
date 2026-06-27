# aprs-stream
A disaggregated APRS pipeline. A single base service demodulates and decodes APRS packets from ka9q-radio RTP audio, then publishes fully-decoded, richly-typed APRS frames onto the local network. Downstream clients subscribe and consume typed frames without ever touching RTP audio or doing AX.25 parsing themselves.

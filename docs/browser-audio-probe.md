# Local Opus playback test

Select **Play test tone** to decode and play a quiet, generated 440 Hz tone.
The six raw mono Opus packets each cover 20 ms at 48 kHz. The page resumes
Web Audio during the click, checks decoded frame size, timestamps, samples
and volume, and waits for the output graph's completion event. Hiding the
page, starting a connection, or cancelling the test retires its decoder and
audio context. The test uses a three-second deadline. A pending context
close blocks another test until cleanup completes.

A pass measures local packet decoding and graph playback. Physical speaker
output requires listening. Remote host audio, a streaming jitter buffer,
browser microphone forwarding, and mobile latency remain open under
`fr-p3-browser-audio-ebf`. The packet representation follows the
[WebCodecs Opus registration](https://www.w3.org/TR/webcodecs-opus-codec-registration/).

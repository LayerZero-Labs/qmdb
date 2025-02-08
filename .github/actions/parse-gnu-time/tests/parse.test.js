const parseGnuTime = require('../parse');

const TEST_INPUT = `
        Command being timed: "ls"
        User time (seconds): 0.00
        System time (seconds): 0.00
        Percent of CPU this job got: 92%
        Elapsed (wall clock) time (h:mm:ss or m:ss): 0:00.00
        Average shared text size (kbytes): 0
        Average unshared data size (kbytes): 0
        Average stack size (kbytes): 0
        Average total size (kbytes): 0
        Maximum resident set size (kbytes): 2028
        Average resident set size (kbytes): 0
        Major (requiring I/O) page faults: 0
        Minor (reclaiming a frame) page faults: 111
        Voluntary context switches: 1
        Involuntary context switches: 0
        Swaps: 0
        File system inputs: 0
        File system outputs: 0
        Socket messages sent: 0
        Socket messages received: 0
        Signals delivered: 0
        Page size (bytes): 4096
        Exit status: 0
`.trim();

describe('parseGnuTime', () => {
  test('basic parsing with specific fields', () => {
    const result = parseGnuTime(TEST_INPUT, ['user_time_seconds', 'system_time_seconds']);
    expect(result).toEqual({
      command: 'ls',
      metrics: {
        user_time_seconds: 0.00,
        system_time_seconds: 0.00
      }
    });
  });

  test('memory conversion', () => {
    const result = parseGnuTime(TEST_INPUT, ['peak_memory_gb']);
    expect(result).toEqual({
      command: 'ls',
      metrics: {
        peak_memory_gb: 0.0
      }
    });
  });

  test('CPU percentage parsing', () => {
    const result = parseGnuTime(TEST_INPUT, ['cpu_usage_percent']);
    expect(result).toEqual({
      command: 'ls',
      metrics: {
        cpu_usage_percent: 92
      }
    });
  });

  test('empty fields returns all metrics', () => {
    const result = parseGnuTime(TEST_INPUT, []);
    expect(result).toEqual({
      command: 'ls',
      metrics: {
        user_time_seconds: 0.00,
        system_time_seconds: 0.00,
        cpu_usage_percent: 92,
        elapsed_time: '0:00.00',
        avg_shared_text_kb: 0,
        avg_unshared_data_kb: 0,
        avg_stack_kb: 0,
        avg_total_kb: 0,
        peak_memory_kb: 2028,
        peak_memory_gb: 0.0,
        avg_shared_text_gb: 0.0,
        avg_unshared_data_gb: 0.0,
        avg_stack_gb: 0.0,
        avg_total_gb: 0.0,
        major_page_faults: 0,
        minor_page_faults: 111,
        voluntary_ctx_switches: 1,
        involuntary_ctx_switches: 0,
        swaps: 0,
        fs_inputs: 0,
        fs_outputs: 0,
        socket_msgs_sent: 0,
        socket_msgs_received: 0,
        signals_delivered: 0,
        page_size_bytes: 4096,
        exit_status: 0
      }
    });
  });
}); 
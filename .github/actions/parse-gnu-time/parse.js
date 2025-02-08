function parseGnuTime(content, fields = []) {
  // Define mappings between time output and our metric names
  const TIME_METRICS = {
    'Command being timed': 'command',
    'User time (seconds)': 'user_time_seconds',
    'System time (seconds)': 'system_time_seconds',
    'Percent of CPU this job got': 'cpu_usage_percent',
    'Elapsed (wall clock) time (h:mm:ss or m:ss)': 'elapsed_time',
    'Maximum resident set size (kbytes)': 'peak_memory_kb',
    'Average shared text size (kbytes)': 'avg_shared_text_kb',
    'Average unshared data size (kbytes)': 'avg_unshared_data_kb',
    'Average stack size (kbytes)': 'avg_stack_kb',
    'Average total size (kbytes)': 'avg_total_kb',
    'Major (requiring I/O) page faults': 'major_page_faults',
    'Minor (reclaiming a frame) page faults': 'minor_page_faults',
    'Voluntary context switches': 'voluntary_ctx_switches',
    'Involuntary context switches': 'involuntary_ctx_switches',
    'Swaps': 'swaps',
    'File system inputs': 'fs_inputs',
    'File system outputs': 'fs_outputs',
    'Socket messages sent': 'socket_msgs_sent',
    'Socket messages received': 'socket_msgs_received',
    'Signals delivered': 'signals_delivered',
    'Page size (bytes)': 'page_size_bytes',
    'Exit status': 'exit_status'
  };

  // Parse metrics
  const parsed = {};
  const lines = content.split('\n');
  for (const _line of lines) {
    const line = _line.trimStart().trimEnd();
    for (const [key, metric] of Object.entries(TIME_METRICS)) {
      const match = line.startsWith(key + ':') ? [line, line.slice(key.length + 1).trim()] : null;
      if (match) {
        let value = match[1].trim();

        // Handle special cases
        if (metric === 'command') {
          value = value.replace(/^"|"$/g, ''); // Remove quotes
        } else if (metric === 'cpu_usage_percent') {
          value = parseFloat(value.replace('%', '')); // Remove % sign and convert to number
        } else if (metric === 'elapsed_time') {
          // Keep elapsed_time as string since it's a time format
          value = value;
        } else if (!isNaN(value)) {
          // Convert any numeric strings to numbers
          value = value.includes('.') ? parseFloat(value) : parseInt(value, 10);
        }

        parsed[metric] = value;
      }
    }
  }

  // Convert KB to GB for memory metrics
  for (const [key, value] of Object.entries(parsed)) {
    if (key.endsWith('_kb')) {
      const gbKey = key.replace('_kb', '_gb');
      parsed[gbKey] = parseFloat((value / 1048576).toFixed(2));
    }
  }

  // Filter metrics if fields are specified
  const filtered = { command: parsed.command };
  if (fields.length > 0) {
    for (const field of fields) {
      if (parsed[field] !== undefined) {
        filtered[field] = parsed[field];
      }
    }
  } else {
    Object.assign(filtered, parsed);
  }

  return {
    command: filtered.command,
    metrics: Object.fromEntries(
      Object.entries(filtered).filter(([key]) => key !== 'command')
    )
  };
}

// When running as a script
if (require.main === module) {
  const core = require('@actions/core');  
  try {
    const content = core.getInput('content', { required: true });
    const fields = JSON.parse(core.getInput('fields') || '[]');

    const result = parseGnuTime(content, fields);
    core.setOutput('json', JSON.stringify(result));
  } catch (error) {
    core.setFailed(error.message);
  }
}

// Export for testing
module.exports = parseGnuTime; 
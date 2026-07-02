import { useState, useCallback, useRef, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';

interface JobStatus {
  job_id:      string;
  document_id: string;
  status:      'queued' | 'running' | 'succeeded' | 'failed';
  attempts:    number;
  error_text:  string | null;
}

/**
 * Polls `cmd_get_job_status` for a given job ID every `intervalMs`
 * milliseconds until it reaches a terminal state (succeeded | failed).
 * Implements the 202-polling pattern from ADR-0010.
 */
export function useJobPoller(intervalMs = 1500) {
  const [jobs, setJobs] = useState<Record<string, JobStatus>>({});
  const timers = useRef<Record<string, ReturnType<typeof setInterval>>>({});

  const track = useCallback((jobId: string) => {
    if (timers.current[jobId]) return; // already tracking

    const id = setInterval(async () => {
      try {
        const status = await invoke<JobStatus | null>('cmd_get_job_status', { jobId });
        if (!status) return; // no job row yet; try again next tick
        setJobs(prev => ({ ...prev, [jobId]: status }));
        if (status.status === 'succeeded' || status.status === 'failed') {
          clearInterval(timers.current[jobId]);
          delete timers.current[jobId];
        }
      } catch (e) {
        console.error('Job poll error:', e);
      }
    }, intervalMs);

    timers.current[jobId] = id;
  }, [intervalMs]);

  // Clean up all timers on unmount.
  useEffect(() => {
    return () => {
      Object.values(timers.current).forEach(clearInterval);
    };
  }, []);

  return { jobs, track };
}

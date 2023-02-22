import axios, { AxiosError } from 'axios';
import { useQuery } from '@tanstack/react-query';
import { useContext } from 'react';
import { LodestoneContext } from './LodestoneContext';

export const useGameSetting = (uuid: string, setting: string, enabled: boolean) => {
  const context = useContext(LodestoneContext);

  return useQuery<string, AxiosError>(
    ['instances', uuid, 'settings', 'game', setting],
    () => {
      return axios
        .get<string>(`/instance/${uuid}/game/${setting}`)
        .then((response) => {
          return response.data;
        });
    },
    {
      enabled: context.token.length > 0 && enabled,
      cacheTime: 0,
      staleTime: 0,
    }
  );
};
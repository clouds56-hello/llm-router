import 'dotenv/config';
import { type DeepSeekLanguageModelOptions, createDeepSeek } from '@ai-sdk/deepseek';
import { generateText, stepCountIs, streamText, tool } from 'ai';
import { ProxyAgent, setGlobalDispatcher } from 'undici';
import { z } from 'zod';

if (!process.env.DEEPSEEK_API_KEY) {
  throw new Error('Missing DEEPSEEK_API_KEY in .env');
}

const deepseek = createDeepSeek({
  apiKey: process.env.DEEPSEEK_API_KEY ?? '',
});

const proxyUrl = process.env.HTTPS_PROXY || process.env.HTTP_PROXY;
if (proxyUrl) {
  setGlobalDispatcher(new ProxyAgent(proxyUrl));
  process.stdout.write(`Using proxy dispatcher: ${proxyUrl}\n`);
}

const getWeatherTool = tool({
  description: 'Get current weather by city',
  inputSchema: z.object({
    city: z.string().describe('City name'),
  }),
  execute: async ({ city }: { city: string }) => {
    return {
      city,
      condition: 'sunny',
      temperatureC: 26,
    };
  },
});

// Example of a multi-step agent loop with tool usage and reasoning

const simpleResult = await generateText({
  model: deepseek('deepseek-chat'),
  prompt: 'Check weather in Beijing and provide one travel tip.',
  tools: {
    getWeather: getWeatherTool,
  },
  providerOptions: {
    deepseek: {
      thinking: { type: 'enabled' },
    } satisfies DeepSeekLanguageModelOptions,
  },
  stopWhen: stepCountIs(5),
});

process.stdout.write(`\nTool Calls: ${JSON.stringify(simpleResult.toolCalls, null, 2)}\n`);
process.stdout.write(`Tool Results: ${JSON.stringify(simpleResult.toolResults, null, 2)}\n`);
process.stdout.write(`reasoningText: ${simpleResult.reasoningText}\n`);
process.stdout.write(`Final: ${simpleResult.text}\n\n\n\n\n\n\n`);

// Example of a multi-step streaming agent loop with reasoning deltas and tool call results

const streamResult = streamText({
  model: deepseek('deepseek-reasoner'),
  prompt: 'Check weather in Beijing and provide one travel tip.',
  tools: {
    getWeather: getWeatherTool,
  },
  providerOptions: {
    deepseek: {
      thinking: { type: 'enabled' },
    } satisfies DeepSeekLanguageModelOptions,
  },
  stopWhen: stepCountIs(5),
});

for await (const part of streamResult.fullStream) {
  if (part.type === 'reasoning-delta') {
    process.stdout.write(`[reasoning] ${part.text}`);
  } else if (part.type === 'text-delta') {
    process.stdout.write(`[text] ${part.text}`);
  } else if (part.type === 'tool-call') {
    process.stdout.write(`\n[tool-call] ${JSON.stringify(part, null, 2)}\n`);
  } else if (part.type === 'tool-result') {
    process.stdout.write(`\n[tool-result] ${JSON.stringify(part, null, 2)}\n`);
  } else if (part.type === 'tool-error') {
    process.stdout.write(`\n[tool-error] ${JSON.stringify(part, null, 2)}\n`);
  }
}

process.stdout.write(`\nStream Tool Calls: ${JSON.stringify(await streamResult.toolCalls, null, 2)}\n`);
process.stdout.write(`Stream Tool Results: ${JSON.stringify(await streamResult.toolResults, null, 2)}\n`);
process.stdout.write(`Stream Reasoning Text: ${await streamResult.reasoningText}\n`);
process.stdout.write(`Stream Final Text: ${await streamResult.text}\n`);
